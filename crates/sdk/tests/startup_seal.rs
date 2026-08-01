//! SDK-started surfnets have no startup tasks, so `Surfnet::start` seals an
//! empty plan up front. An unsealed plan would project a forever-pending
//! `surfpool-startup` execution into `getSurfnetInfo`, starving
//! readiness-checking clients (legacy Anchor's readiness loop has no
//! timeout); this test pins the seal in place.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

use surfpool_sdk::Surfnet;

/// Minimal JSON-RPC POST over a raw socket: HTTP/1.0 so the response is
/// unchunked and terminated by connection close.
fn get_surfnet_info(rpc_url: &str) -> serde_json::Value {
    let address = rpc_url.trim_start_matches("http://");
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"surfnet_getSurfnetInfo"}"#;
    let request = format!(
        "POST / HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let attempt = TcpStream::connect(address).and_then(|mut stream| {
            stream.write_all(request.as_bytes())?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        });
        match attempt {
            Ok(response) => {
                let json_start = response.find("\r\n\r\n").expect("malformed HTTP response") + 4;
                return serde_json::from_str(&response[json_start..])
                    .expect("response body should be JSON");
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "getSurfnetInfo unreachable at {address}: {error}"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// Multi-threaded flavor: Surfnet::start uses a blocking RpcClient while
// waiting for startup airdrops, which panics on a current-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn sdk_surfnets_report_a_ready_startup() {
    let mut surfnet = Surfnet::start().await.expect("surfnet should start");

    let info = get_surfnet_info(surfnet.rpc_url());
    let value = &info["result"]["value"];
    assert_eq!(
        value["startup"]["phase"], "ready",
        "startup should be sealed and ready: {info}"
    );
    assert_eq!(value["startup"]["planSealed"], true);
    // The legacy-Anchor-visible part: no phantom pending execution.
    assert_eq!(
        value["runbookExecutions"]
            .as_array()
            .expect("runbookExecutions should be an array")
            .len(),
        0,
        "no compat entry should remain once startup is ready: {info}"
    );

    surfnet.stop().expect("surfnet should stop cleanly");
}
