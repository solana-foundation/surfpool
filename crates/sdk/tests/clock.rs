//! A snapshot restores accounts, not the clock. `time_travel_to_slot_at`
//! restores both the slot and the unix time the snapshot was taken at, which
//! programs that compare the two (oracle staleness, funding) depend on.

use solana_client::rpc_request::RpcRequest;
use surfpool_sdk::Surfnet;

#[tokio::test(flavor = "multi_thread")]
async fn time_travel_to_slot_at_sets_both_slot_and_unix_time() {
    let surfnet = Surfnet::start().await.unwrap();
    let slot = 462_246_025;
    let timestamp_ms: u64 = 1_761_523_200_000;

    let epoch_info = surfnet
        .cheatcodes()
        .time_travel_to_slot_at(slot, timestamp_ms)
        .unwrap();
    assert_eq!(epoch_info.absolute_slot, slot);

    // The Clock sysvar is what programs read.
    let clock = surfnet
        .rpc_client()
        .send::<serde_json::Value>(
            RpcRequest::GetAccountInfo,
            serde_json::json!([
                "SysvarC1ock11111111111111111111111111111111",
                { "encoding": "jsonParsed", "commitment": "processed" }
            ]),
        )
        .unwrap();
    let info = &clock["value"]["data"]["parsed"]["info"];
    assert_eq!(info["slot"].as_u64(), Some(slot), "{clock}");
    let unix_timestamp = info["unixTimestamp"].as_i64().unwrap();
    assert!(
        (unix_timestamp - (timestamp_ms / 1_000) as i64).abs() < 60,
        "unix time {unix_timestamp} should be close to {}",
        timestamp_ms / 1_000
    );
}
