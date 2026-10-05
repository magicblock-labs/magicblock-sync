use std::{sync::Arc, time::Duration};

use engine::testkit::TestEngine;
use hyper::StatusCode;
use keeper::testkit::{store_v42, v42_builder, V42_ID};
use solana_account::{AccountMode, ReadableAccount};
use solana_instruction::{AccountMeta, Instruction};

use super::{
    coverage::{Coverage, Source},
    lifecycle::PreparedDelegation,
    *,
};
use crate::transport::magic::intents;
use crate::transport::{
    config, delegation as delegation_images, http::Server, magic, scripted, within, Account, Ata,
    DELEGATION_SLOT, PROJECTION_SLOT, SERVICE_ERROR, SNAPSHOT_SLOT, UNAVAILABLE, V42_INITIAL,
};
use crate::websocket::Event;
use crate::{aml, ata, AccountSubscription, AmlConfig, Error};

const LOCAL_BALANCE: i64 = 9;
const NEWER_SLOT: u64 = 20;
const BUFFERED_SLOT: u64 = 100;

/// Private event handlers need no live gRPC stream; only HTTP/AML scenarios start a provider.
fn start_chain_sync(
    engine: &mut TestEngine,
    http: Option<&Server>,
    with_aml: bool,
) -> Arc<ChainSync> {
    let unavailable = UNAVAILABLE.parse().unwrap();
    let endpoint = http.map_or(&unavailable, |server| &server.endpoint);
    let mut config = config(endpoint, None, &[&unavailable]);
    config.aml = with_aml.then(|| AmlConfig {
        endpoint: endpoint.clone(),
        timeout: Duration::from_secs(5),
    });
    let handle = engine.clone();
    ChainSync::new(handle, config, engine.shutdown()).unwrap()
}

/// Ordinary, confined, and projected gRPC delegations retire tracking and reject buffered updates and confirmations.
#[tokio::test]
async fn grpc_delegation_retires_tracking_and_rejects_buffered_mirror_updates() {
    let mut engine = TestEngine::new().await;
    let chain_sync = start_chain_sync(&mut engine, None, false);
    let mut cases = Vec::new();
    for mode in [AccountMode::Delegated, AccountMode::Magic] {
        let key = Pubkey::new_unique();
        engine
            .accounts()
            .store(&[(
                key,
                v42_builder(LOCAL_BALANCE, AccountMode::Uninit).slot(SNAPSHOT_SLOT).build(),
            )])
            .unwrap();
        let authority =
            if mode == AccountMode::Magic { Pubkey::default() } else { engine.authority() };
        let (_, record) = delegation_images(key, V42_ID, authority, DELEGATION_SLOT, &[]);
        let mut account = v42_builder(V42_INITIAL, mode).slot(DELEGATION_SLOT);
        if mode == AccountMode::Magic {
            account = account.lamports(0);
        }
        cases.push((
            key,
            grpc::Delegation {
                pubkey: key,
                account,
                source_program: V42_ID,
                record: record.data,
            },
        ));
    }
    let Ata {
        ata: key,
        base,
        eata,
        delegated: account,
        record,
    } = Ata::new(
        spl_token_interface::id(),
        false,
        engine.authority(),
        PROJECTION_SLOT,
    );
    let lamports = engine.rent().minimum_balance(base.read().data().len());
    let base = base.lamports(lamports);
    engine.accounts().store(&[(key, base.build())]).unwrap();
    cases.push((
        key,
        grpc::Delegation {
            pubkey: eata,
            source_program: account.read().owner(),
            account,
            record,
        },
    ));
    for (key, delegation) in cases {
        let mode = delegation.account.read().mode();
        let slot = delegation.account.read().slot();
        let sub = AccountSubscription { pubkey: key, program: None };
        let mut coverage = Coverage::default();
        let generation = coverage.acknowledged(sub);
        coverage.confirmed(0, key, generation);
        if mode == AccountMode::Magic {
            // Delegation must also retire tracking when only its gRPC copy remains.
            coverage.lost(Source::WebSocket, key);
        }
        within(chain_sync.on_grpc(grpc::Event::Delegated(delegation), &mut coverage))
            .await
            .unwrap();
        let accepted = engine.get_account(key).unwrap();
        assert_eq!(accepted.mode(), mode);
        assert_eq!(accepted.slot(), slot);
        // An old filter acknowledgement cannot re-enable coverage after delegation.
        coverage.confirmed(0, key, generation);
        assert!(!coverage.ws_contains(sub));
        assert!(!coverage.grpc_contains(0, sub));
        chain_sync
            .on_websocket(
                Event::Update {
                    sub,
                    account: v42_builder(LOCAL_BALANCE, AccountMode::Uninit).slot(BUFFERED_SLOT),
                },
                &mut coverage,
            )
            .await
            .unwrap();
        chain_sync
            .on_grpc(
                grpc::Event::Update {
                    stream: 0,
                    sub,
                    account: v42_builder(LOCAL_BALANCE, AccountMode::Uninit)
                        .slot(BUFFERED_SLOT + 1),
                },
                &mut coverage,
            )
            .await
            .unwrap();
        assert_eq!(engine.get_account(key).unwrap(), accepted);
    }
    assert!(engine.get_account(eata).is_none());
    drop(chain_sync);
    engine.close().await;
}

/// AML failures do not rescue; distinct rejected signers and invalid actions each schedule one rescue.
#[tokio::test]
async fn aml_errors_skip_rescue_but_rejected_signers_and_invalid_actions_schedule_it() {
    let mut engine = magic::engine().await;
    let mut http = Server::new().await;
    let chain_sync = start_chain_sync(&mut engine, Some(&http), true);
    for (status, body) in
        [(StatusCode::SERVICE_UNAVAILABLE, SERVICE_ERROR), (StatusCode::OK, "invalid")]
    {
        let key = Pubkey::new_unique();
        let prepared = PreparedDelegation {
            pubkey: key,
            account: v42_builder(V42_INITIAL, AccountMode::Delegated).slot(DELEGATION_SLOT),
            source_program: V42_ID,
            actions: Ok(Some(vec![Instruction::new_with_bytes(
                V42_ID,
                &[],
                vec![AccountMeta::new(key, true)],
            )])),
        };
        let script = async {
            http.next().await.respond(status, body);
        };
        let result = scripted(chain_sync.materialize_delegation(prepared), script).await;
        assert!(matches!(
            result,
            Err(Error::Aml(aml::Error::Status(_) | aml::Error::Json(_)))
        ));
        assert!(engine.get_account(key).is_none());
        assert_eq!(intents(&engine), 0);
    }

    let key = Pubkey::new_unique();
    let approved_signer = Pubkey::new_unique();
    let action = Instruction::new_with_bytes(
        V42_ID,
        &[],
        vec![
            AccountMeta::new(key, true),
            AccountMeta::new(key, true),
            AccountMeta::new(approved_signer, true),
            AccountMeta::new(Pubkey::new_unique(), false),
        ],
    );
    let prepared = PreparedDelegation {
        pubkey: key,
        account: v42_builder(V42_INITIAL, AccountMode::Delegated).slot(DELEGATION_SLOT),
        source_program: V42_ID,
        // Duplicates within and across actions must still assess each signer once.
        actions: Ok(Some(vec![action.clone(), action])),
    };
    let script = async {
        let mut assessed = Vec::new();
        for _ in 0..2 {
            let call = http.next().await;
            let signer = call.uri.split("pubkey=").nth(1).unwrap().parse::<Pubkey>().unwrap();
            assessed.push(signer);
            let body = if signer == key { r#"{"isRisky":true}"# } else { r#"{"isRisky":false}"# };
            call.respond(StatusCode::OK, body);
        }
        assessed.sort_unstable();
        let mut expected = [key, approved_signer];
        expected.sort_unstable();
        assert_eq!(assessed, expected);
    };
    let result = scripted(chain_sync.materialize_delegation(prepared), script).await;
    result.unwrap();
    assert!(!http.pending());
    assert_eq!(
        engine.get_account(key).unwrap().mode(),
        AccountMode::Transient
    );
    assert_eq!(intents(&engine), 1);
    let invalid = Pubkey::new_unique();
    let (_, mut record) =
        delegation_images(invalid, V42_ID, engine.authority(), DELEGATION_SLOT, &[]);
    record.data.push(255);
    let delegation = grpc::Delegation {
        pubkey: invalid,
        account: v42_builder(V42_INITIAL, AccountMode::Delegated).slot(DELEGATION_SLOT),
        source_program: V42_ID,
        record: record.data,
    };
    let (prepared, dependencies) = chain_sync.prepare_delegation(delegation).unwrap();
    assert!(dependencies.is_empty());
    assert!(matches!(&prepared.actions, Err(Error::Actions(_))));
    within(chain_sync.materialize_delegation(prepared)).await.unwrap();
    assert_eq!(
        engine.get_account(invalid).unwrap().mode(),
        AccountMode::Transient
    );
    assert_eq!(intents(&engine), 2);
    drop(chain_sync);
    engine.close().await;
    http.close().await;
}

/// Newer, confined, and same-slot Transient state suppress obsolete rescue without scheduling an intent.
#[tokio::test]
async fn newer_confined_and_transient_state_suppress_obsolete_rescue() {
    let mut engine = magic::engine().await;
    let chain_sync = start_chain_sync(&mut engine, None, false);
    for (mode, slot) in [
        (AccountMode::Delegated, DELEGATION_SLOT + 1),
        (AccountMode::Magic, DELEGATION_SLOT - 1),
        (AccountMode::Transient, DELEGATION_SLOT),
    ] {
        let key = Pubkey::new_unique();
        engine
            .accounts()
            .store(&[(key, v42_builder(LOCAL_BALANCE, mode).slot(slot).build())])
            .unwrap();
        within(chain_sync.rescue_delegation(
            key,
            v42_builder(V42_INITIAL, AccountMode::Delegated).slot(DELEGATION_SLOT),
            V42_ID,
        ))
        .await
        .unwrap();
        assert_eq!(engine.get_account(key).unwrap().mode(), mode);
        assert_eq!(
            engine.get_account(key).unwrap().data(),
            LOCAL_BALANCE.to_le_bytes()
        );
    }
    assert_eq!(intents(&engine), 0);
    drop(chain_sync);
    engine.close().await;
}

/// Requests transition once; confirmed releases delete eligible state but preserve newer and authoritative accounts.
#[tokio::test]
async fn undelegation_transitions_once_and_confirmations_preserve_newer_or_authoritative_state() {
    let mut engine = magic::engine().await;
    let chain_sync = start_chain_sync(&mut engine, None, false);
    let key = Pubkey::new_unique();
    engine
        .accounts()
        .store(&[(
            key,
            v42_builder(V42_INITIAL, AccountMode::Delegated).slot(DELEGATION_SLOT).build(),
        )])
        .unwrap();
    within(chain_sync.undelegation_requested(key, DELEGATION_SLOT - 1))
        .await
        .unwrap();
    assert_eq!(intents(&engine), 0);
    within(chain_sync.undelegation_requested(key, DELEGATION_SLOT)).await.unwrap();
    assert_eq!(
        engine.get_account(key).unwrap().mode(),
        AccountMode::Transient
    );
    within(chain_sync.undelegation_requested(key, DELEGATION_SLOT + 1))
        .await
        .unwrap();
    assert_eq!(intents(&engine), 1);
    let readonly = store_v42(&engine, V42_INITIAL, AccountMode::ReadOnly);
    within(chain_sync.undelegation_requested(readonly, BUFFERED_SLOT))
        .await
        .unwrap();
    assert_eq!(intents(&engine), 1);
    within(chain_sync.undelegated(key, DELEGATION_SLOT - 1)).await.unwrap();
    assert_eq!(
        engine.get_account(key).unwrap().mode(),
        AccountMode::Transient
    );
    within(chain_sync.undelegated(key, DELEGATION_SLOT)).await.unwrap();
    assert!(engine.get_account(key).is_none());
    for (mode, observed, removed) in [
        (AccountMode::Uninit, DELEGATION_SLOT, true),
        (AccountMode::Uninit, DELEGATION_SLOT + 1, false),
        (AccountMode::Delegated, DELEGATION_SLOT, false),
        (AccountMode::Magic, DELEGATION_SLOT, false),
    ] {
        let key = Pubkey::new_unique();
        engine
            .accounts()
            .store(&[(key, v42_builder(V42_INITIAL, mode).slot(observed).build())])
            .unwrap();
        within(chain_sync.undelegated(key, DELEGATION_SLOT)).await.unwrap();
        assert_eq!(engine.get_account(key).is_none(), removed);
    }
    drop(chain_sync);
    engine.close().await;
}

/// Obsolete updates cannot resurrect coverage; explicit release preserves a reacquired mirror and rejects buffered updates.
#[tokio::test]
async fn stale_confirmations_wrong_streams_and_removed_subscriptions_reject_updates() {
    let mut engine = TestEngine::new().await;
    let chain_sync = start_chain_sync(&mut engine, None, false);
    let key = Pubkey::new_unique();
    let sub = AccountSubscription { pubkey: key, program: None };
    let mut coverage = Coverage::default();
    let old_generation = coverage.acknowledged(sub);
    coverage.remove(sub);
    let current_generation = coverage.acknowledged(sub);
    let mirror_update = |stream| grpc::Event::Update {
        stream,
        sub,
        account: v42_builder(V42_INITIAL, AccountMode::Uninit).slot(DELEGATION_SLOT),
    };
    // A delayed confirmation for the previous acquisition cannot grant coverage.
    coverage.confirmed(0, key, old_generation);
    chain_sync.on_grpc(mirror_update(0), &mut coverage).await.unwrap();
    assert!(engine.get_account(key).is_none());
    coverage.confirmed(0, key, current_generation);
    chain_sync.on_grpc(mirror_update(1), &mut coverage).await.unwrap();
    assert!(engine.get_account(key).is_none());
    coverage.remove(sub);
    chain_sync.on_grpc(mirror_update(0), &mut coverage).await.unwrap();
    assert!(engine.get_account(key).is_none());
    coverage.acknowledged(sub);
    engine
        .accounts()
        .store(&[(
            key,
            v42_builder(LOCAL_BALANCE, AccountMode::Uninit).slot(NEWER_SLOT).build(),
        )])
        .unwrap();
    // Removal revokes coverage only. Deletion belongs to the eviction caller,
    // so delayed transport cleanup must not delete a newly installed snapshot.
    chain_sync.on_websocket(Event::Removed(key), &mut coverage).await.unwrap();
    chain_sync
        .on_websocket(
            Event::Update {
                sub,
                account: v42_builder(V42_INITIAL, AccountMode::Uninit).slot(NEWER_SLOT + 1),
            },
            &mut coverage,
        )
        .await
        .unwrap();
    let account = engine
        .get_account(key)
        .expect("old cleanup must preserve the reacquired mirror");
    assert_eq!(account.data(), LOCAL_BALANCE.to_le_bytes());
    assert_eq!(account.slot(), NEWER_SLOT);
    drop(coverage);
    drop(chain_sync);
    engine.close().await;
}

/// Confirmed raw-eATA undelegation deletes its eligible virtual ATA without ever materializing the eATA.
#[tokio::test]
async fn raw_eata_undelegation_deletes_only_eligible_ata_projection() {
    let mut engine = TestEngine::new().await;
    let mut http = Server::new().await;
    let chain_sync = start_chain_sync(&mut engine, Some(&http), false);
    let authority = engine.authority();
    let Ata {
        ata: key,
        base,
        eata,
        delegated,
        record,
    } = Ata::new(spl_token_interface::id(), false, authority, PROJECTION_SLOT);
    let projected = ata::project(key, base, eata, &delegated, &record, authority)
        .unwrap()
        .mode(AccountMode::Transient);
    engine.accounts().store(&[(key, projected.build())]).unwrap();
    let raw = Account::from_builder(eata, &delegated);
    let script = async {
        let call = http.next().await;
        assert_eq!(call.body["params"][1]["minContextSlot"], PROJECTION_SLOT);
        call.snapshot(PROJECTION_SLOT, &[Some(raw)]);
    };
    let result = scripted(chain_sync.undelegated(eata, PROJECTION_SLOT), script).await;
    result.unwrap();
    assert!(engine.get_account(key).is_none());
    assert!(engine.get_account(eata).is_none());
    drop(chain_sync);
    engine.close().await;
    http.close().await;
}

/// Last-source loss evicts mirrors but cannot delete delegated, confined, or already-transient accounts.
#[tokio::test]
async fn last_source_loss_evicts_only_non_authoritative_accounts() {
    let mut engine = TestEngine::new().await;
    let chain_sync = start_chain_sync(&mut engine, None, false);
    for (mode, preserved) in [
        (AccountMode::Uninit, false),
        (AccountMode::ReadOnly, false),
        (AccountMode::Delegated, true),
        (AccountMode::Magic, true),
        (AccountMode::Transient, true),
    ] {
        let key = Pubkey::new_unique();
        engine
            .accounts()
            .store(&[(
                key,
                v42_builder(LOCAL_BALANCE, mode).slot(DELEGATION_SLOT).build(),
            )])
            .unwrap();
        let mut coverage = Coverage::default();
        let sub = AccountSubscription { pubkey: key, program: None };
        let generation = coverage.acknowledged(sub);
        coverage.confirmed(0, key, generation);
        within(chain_sync.lost(&mut coverage, Source::WebSocket, key)).await.unwrap();
        assert!(engine.get_account(key).is_some());
        within(chain_sync.lost(&mut coverage, Source::Grpc, key)).await.unwrap();
        assert_eq!(engine.get_account(key).is_some(), preserved);
    }
    drop(chain_sync);
    engine.close().await;
}

/// Source loss preserves remaining coverage; last loss removes it and stale generations cannot restore it.
#[test]
fn coverage_survives_partial_loss_and_rejects_obsolete_generations() {
    for source in [Source::WebSocket, Source::Grpc] {
        let mut coverage = Coverage::default();
        let sub = AccountSubscription {
            pubkey: Pubkey::new_unique(),
            program: None,
        };
        let generation = coverage.acknowledged(sub);
        coverage.confirmed(2, sub.pubkey, generation);
        assert!(coverage.ws_contains(sub));
        assert!(coverage.grpc_contains(2, sub));
        assert!(!coverage.grpc_contains(1, sub));
        let wrong = AccountSubscription {
            program: Some(Pubkey::new_unique()),
            ..sub
        };
        assert!(!coverage.ws_contains(wrong));
        assert!(!coverage.grpc_contains(2, wrong));
        assert_eq!(coverage.lost(source, sub.pubkey), (false, None));
        let remaining = match source {
            Source::WebSocket => Source::Grpc,
            Source::Grpc => Source::WebSocket,
        };
        assert_eq!(
            coverage.lost(remaining, sub.pubkey),
            (true, Some(sub.pubkey))
        );
        assert!(!coverage.ws_contains(sub));
        assert!(!coverage.grpc_contains(2, sub));
        coverage.confirmed(2, sub.pubkey, generation);
        assert!(!coverage.grpc_contains(2, sub));
        let current = coverage.acknowledged(sub);
        assert_ne!(generation, current);
        coverage.confirmed(0, sub.pubkey, generation);
        assert!(!coverage.grpc_contains(0, sub));
        coverage.confirmed(1, sub.pubkey, current);
        coverage.confirmed(0, sub.pubkey, generation);
        assert!(!coverage.grpc_contains(0, sub));
        assert!(coverage.grpc_contains(1, sub));
        assert!(coverage.remove(sub));
        coverage.confirmed(1, sub.pubkey, current);
        assert!(!coverage.grpc_contains(1, sub));
    }
}

/// Program and ProgramData feed the same local target, which is evicted only after both disappear.
#[test]
fn program_and_program_data_loss_evict_the_same_local_program() {
    let program = Pubkey::new_unique();
    let [direct, data] = AccountSubscription::for_account(program);
    for [first, last] in [[direct, data], [data, direct]] {
        let mut coverage = Coverage::default();
        coverage.acknowledged(first);
        coverage.acknowledged(last);
        assert_eq!(coverage.removed(first.pubkey), None);
        assert_eq!(coverage.removed(last.pubkey), Some(program));
    }
}
