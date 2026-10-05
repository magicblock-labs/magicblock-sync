use engine::testkit::TestEngine;
use hyper::StatusCode;
use solana_pubkey::Pubkey;

use super::*;
use crate::transport::{http::Server, scripted, Account, SERVICE_ERROR};

const FRESHNESS_FLOOR: u64 = 90;
const CLOCK_SLOT: u64 = 95;
const RESPONSE_SLOT: u64 = 100;

/// Transient errors rotate providers without weakening freshness or alignment; malformed responses stop retries.
#[tokio::test]
async fn failover_preserves_freshness_and_null_alignment_but_malformed_responses_stop() {
    let engine = TestEngine::new().await;
    engine.accounts().advance_chain_slot(80);
    let mut first = Server::new().await;
    let mut second = Server::new().await;
    let fetcher = Fetcher::new(
        vec![first.endpoint.clone(), second.endpoint.clone()],
        engine.clone(),
    )
    .unwrap();
    let keys = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
    let scenario = async {
        let call = first.next().await;
        assert_eq!(call.body["params"][1]["minContextSlot"], FRESHNESS_FLOOR);
        // One acquisition carries a fixed floor across retries, even if Clock
        // moves while the first provider's response is held.
        engine.accounts().advance_chain_slot(CLOCK_SLOT);
        call.respond(StatusCode::SERVICE_UNAVAILABLE, SERVICE_ERROR);
        let call = second.next().await;
        assert_eq!(call.body["params"][1]["minContextSlot"], FRESHNESS_FLOOR);
        assert_eq!(call.body["params"][0][1], keys[1].to_string());
        call.snapshot(
            RESPONSE_SLOT,
            &[Some(Account::new(keys[0])), None, Some(Account::new(keys[2]))],
        );
    };
    let result = scripted(fetcher.fetch(&keys, Some(FRESHNESS_FLOOR)), scenario).await;
    let snapshot = result.unwrap();
    assert_eq!(snapshot.slot, RESPONSE_SLOT);
    assert!(snapshot.accounts[0].is_some());
    assert!(snapshot.accounts[1].is_none());
    assert!(snapshot.accounts[2].is_some());
    assert_eq!(
        engine.accounts().chain_slot(),
        CLOCK_SLOT,
        "HTTP must not advance the Clock watermark"
    );
    drop(fetcher);

    // Invalid JSON, missing result, and wrong response length are provider data
    // errors, not transient availability failures eligible for rotation.
    for body in ["{", "{}", r#"{"result":{"context":{"slot":1},"value":[]}}"#] {
        let fetcher = Fetcher::new(
            vec![first.endpoint.clone(), second.endpoint.clone()],
            engine.clone(),
        )
        .unwrap();
        let keys = [Pubkey::new_unique()];
        let script = async {
            first.next().await.respond(StatusCode::OK, body);
        };
        let result = scripted(fetcher.fetch(&keys, None), script).await;
        assert!(result.is_err());
        assert!(!second.pending());
    }
    first.close().await;
    second.close().await;
    engine.close().await;
}
