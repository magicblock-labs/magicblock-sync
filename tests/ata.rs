//! HTTP and streamed delegation share canonical ATA projection and confirmed-release behavior.

mod transport;

use engine::testkit::TestEngine;
use magicblock_chainsync::{AccountProperty, ChainSyncAccount};
use solana_account::{AccountBuilder, AccountMode, ReadableAccount};
use solana_program_pack::Pack;
use spl_token_interface::state::Account as TokenAccount;
use transport::{
    acquire, http, scripted, start_chain_sync, websocket, yellowstone, Account, Ata,
    DELEGATED_TOKEN_BALANCE, DELEGATION_SLOT, FIRST_SUBSCRIPTION, READY_SLOT, TOKEN_BALANCE,
};

/// Both acquisition paths project balances and retire tracking; confirmed eATA release deletes only the projection.
#[tokio::test]
async fn http_and_grpc_delegations_project_ata_and_release_without_raw_eata() {
    let mut engine = TestEngine::new().await;
    let mut http = http::Server::new().await;
    let mut ws = websocket::Server::new().await;
    let grpc = yellowstone::Server::new().await;
    let chain_sync = start_chain_sync(&mut engine, &http, Some(&ws), &grpc);
    ws.wait_for_connection().await;
    grpc.delegation_barrier(&engine, READY_SLOT).await;

    for via_grpc in [false, true] {
        let slot = if via_grpc { DELEGATION_SLOT + 10 } else { DELEGATION_SLOT };
        let snapshot_slot = if via_grpc { slot - 5 } else { slot + 2 };
        let subscription = FIRST_SUBSCRIPTION + u64::from(via_grpc);
        let mut fixture = Ata::new(spl_token_interface::id(), false, engine.authority(), slot);
        let rent = engine.rent().minimum_balance(TokenAccount::LEN);
        fixture.base = fixture.base.lamports(rent);
        let [application, record] = fixture.delegation();
        let request = ChainSyncAccount {
            pubkey: fixture.ata,
            property: AccountProperty::Readonly,
        };
        let script = async {
            ws.next().await.ack(subscription);
            http.next().await.snapshot(
                snapshot_slot,
                &[Some(Account::from_builder(fixture.ata, &fixture.base))],
            );
            let call = http.next().await;
            assert_eq!(call.body["params"][0][0], fixture.eata.to_string());
            assert_eq!(call.body["params"][0][1], record.key.to_string());
            assert_eq!(call.body["params"][1]["minContextSlot"], snapshot_slot);
            // The gRPC path first acquires an ordinary ATA with absent companions;
            // the HTTP path discovers its eATA delegation in this same acquisition.
            let images = if via_grpc {
                [None, None]
            } else {
                [Some(application.clone()), Some(record.clone())]
            };
            call.snapshot(snapshot_slot + 1, &images);
        };
        scripted(acquire(&chain_sync, &[request]), script).await.unwrap();
        if via_grpc {
            let base = engine.get_account(fixture.ata).unwrap();
            assert_eq!(
                TokenAccount::unpack(base.data()).unwrap().amount,
                TOKEN_BALANCE
            );
            grpc.confirmed(slot, &[application, record]);
        }
        grpc.delegation_barrier(&engine, slot + 1).await;
        let projected = engine.get_account(fixture.ata).unwrap();
        assert_eq!(projected.mode(), AccountMode::Delegated);
        assert_eq!(projected.slot(), slot);
        assert_eq!(projected.owner(), &spl_token_interface::id());
        assert_eq!(projected.lamports(), rent);
        assert_eq!(
            TokenAccount::unpack(projected.data()).unwrap().amount,
            DELEGATED_TOKEN_BALANCE
        );
        assert_eq!(ws.unsubscribed().await, subscription);
        assert!(engine.get_account(fixture.eata).is_none());

        // Real commit scheduling belongs in RedSuite; retain the actual projection
        // and seed the eligible local boundary before testing the streamed release.
        let transient = AccountBuilder::from(projected).mode(AccountMode::Transient).build();
        engine.accounts().store(&[(fixture.ata, transient)]).unwrap();
        grpc.undelegated(slot + 2, fixture.eata);
        let script = async {
            let call = http.next().await;
            assert_eq!(call.body["params"][0][0], fixture.eata.to_string());
            assert_eq!(call.body["params"][1]["minContextSlot"], slot + 2);
            let raw = Account::from_builder(fixture.eata, &fixture.delegated);
            call.snapshot(slot + 2, &[Some(raw)]);
        };
        scripted(grpc.delegation_barrier(&engine, slot + 3), script).await;
        assert!(engine.get_account(fixture.ata).is_none());
        assert!(engine.get_account(fixture.eata).is_none());
        assert!(
            !http.pending(),
            "outer/CPI duplicates must produce only one lookup"
        );
    }
    drop(chain_sync);
    engine.close().await;
    ws.close().await;
    http.close().await;
    grpc.close().await;
}
