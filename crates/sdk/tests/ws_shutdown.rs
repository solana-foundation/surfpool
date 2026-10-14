use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use surfpool_sdk::Surfnet;

const SERVER_NORMAL_CLOSE: [u8; 4] = [0x88, 0x02, 0x03, 0xE8];
const CLIENT_NORMAL_CLOSE_MASKED: [u8; 8] = [0x88, 0x82, 0, 0, 0, 0, 0x03, 0xE8];

fn open_websocket(ws_url: &str) -> TcpStream {
    let address = ws_url.trim_start_matches("ws://");
    let mut stream = TcpStream::connect(address).expect("ws port should accept connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let handshake = format!(
        "GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(handshake.as_bytes()).unwrap();

    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("handshake response");
        response.push(byte[0]);
    }
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "the upgrade should be accepted, got: {response}"
    );
    stream
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_closes_websockets_with_a_normal_close_frame() {
    let mut surfnet = Surfnet::start().await.expect("surfnet should start");
    let mut stream = open_websocket(surfnet.ws_url());

    let stop = tokio::task::spawn_blocking(move || surfnet.stop());

    let mut frame = [0u8; 4];
    stream
        .read_exact(&mut frame)
        .expect("the server should send a close frame before dropping the socket");
    assert_eq!(frame, SERVER_NORMAL_CLOSE);

    stream.write_all(&CLIENT_NORMAL_CLOSE_MASKED).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "nothing should follow the close frame");

    stop.await.unwrap().expect("surfnet should stop cleanly");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_does_not_wait_forever_on_unanswered_close_frames() {
    let mut surfnet = Surfnet::start().await.expect("surfnet should start");
    let mut stream = open_websocket(surfnet.ws_url());

    let stop = tokio::task::spawn_blocking(move || surfnet.stop());

    let mut frame = [0u8; 4];
    stream
        .read_exact(&mut frame)
        .expect("the server should send a close frame before dropping the socket");
    assert_eq!(frame, SERVER_NORMAL_CLOSE);

    tokio::time::timeout(Duration::from_secs(4), stop)
        .await
        .expect("stop should give up on the close handshake after the grace period")
        .unwrap()
        .expect("surfnet should stop cleanly");

    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(
        rest.is_empty(),
        "the server should drop the socket after the grace period"
    );
}
