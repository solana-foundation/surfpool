//! A surfnet started with a secret path answers only at that path: a process
//! that can reach the port but doesn't know the secret gets a 404 for every
//! request, standard methods included, on HTTP and WebSocket alike.

use std::{
    io::{Read, Write},
    net::TcpStream,
};

use surfpool_sdk::Surfnet;

const SECRET: &str = "7f3c9a1e5b2d4c6a8e0f1a3b5c7d9e1f";

/// `host:port` and the path of an `http://` or `ws://` URL.
fn split(url: &str) -> (String, String) {
    let rest = url.split_once("://").expect("a scheme").1;
    match rest.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    }
}

/// The status code of a raw HTTP/1.1 request.
fn status(host: &str, request: String) -> u16 {
    let mut stream = TcpStream::connect(host).expect("the port is open");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = [0u8; 64];
    let n = stream.read(&mut response).unwrap();
    let line = String::from_utf8_lossy(&response[..n]);
    line.split_whitespace()
        .nth(1)
        .expect("a status line")
        .parse()
        .unwrap()
}

fn post(host: &str, path: &str) -> u16 {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"getSlot"}"#;
    status(
        host,
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn ws_handshake(host: &str, path: &str) -> u16 {
    status(
        host,
        format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_secret_path_is_served() {
    let surfnet = Surfnet::builder()
        .secret_path(SECRET)
        .start()
        .await
        .expect("surfnet starts");

    // Everything built from the surfnet's own URL works.
    assert!(surfnet.rpc_url().ends_with(&format!("/{SECRET}")));
    assert!(surfnet.ws_url().ends_with(&format!("/{SECRET}")));
    surfnet
        .rpc_client()
        .get_slot()
        .expect("the SDK's client reaches the surfnet");
    surfnet
        .cheatcodes()
        .time_travel_to_slot(1_000)
        .expect("cheatcodes reach the surfnet");

    let (host, path) = split(surfnet.rpc_url());
    let (ws_host, ws_path) = split(surfnet.ws_url());
    tokio::task::spawn_blocking(move || {
        assert_eq!(post(&host, "/"), 404, "the root path is not served");
        let wrong = format!("/{}", "0".repeat(SECRET.len()));
        assert_eq!(post(&host, &wrong), 404, "a wrong secret is not served");
        assert_eq!(post(&host, &path), 200, "the secret path is served");
        assert_ne!(
            ws_handshake(&ws_host, "/"),
            101,
            "the WebSocket root path is not served"
        );
        assert_eq!(
            ws_handshake(&ws_host, &ws_path),
            101,
            "the WebSocket secret path is served"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_secret_every_path_is_served() {
    let surfnet = Surfnet::builder().start().await.expect("surfnet starts");
    let (host, _) = split(surfnet.rpc_url());
    let status = tokio::task::spawn_blocking(move || post(&host, "/anything"))
        .await
        .unwrap();
    assert_eq!(status, 200);
}
