#![cfg(unix)]

use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

struct Surfpool {
    child: Child,
    directory: TempDir,
    ports: [u16; 3],
}

impl Surfpool {
    fn start() -> Self {
        Self::with_snapshot(8, "AAAA".repeat(1024 * 1024))
    }

    fn with_snapshot(count: u32, data: String) -> Self {
        let directory = tempfile::tempdir().unwrap();
        // Decode real accounts after HTTP starts, widening the startup window
        // without replacing execution or adding test hooks to the CLI.
        let account = serde_json::json!({
            "lamports": 1_000_000,
            "owner": "11111111111111111111111111111111",
            "data": data,
            "executable": false,
            "rentEpoch": 0
        });
        let mut snapshot = serde_json::Map::new();
        for index in 1..=count {
            snapshot.insert(snapshot_key(index), account.clone());
        }
        let snapshot_path = directory.path().join("snapshot.json");
        fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let listeners = [0; 3].map(|_| TcpListener::bind("127.0.0.1:0").unwrap());
        let ports = listeners
            .each_ref()
            .map(|listener| listener.local_addr().unwrap().port());
        drop(listeners);
        let log = File::create(directory.path().join("surfpool.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_surfpool"))
            .args([
                "start",
                "--offline",
                "--no-deploy",
                "--no-tui",
                "--no-studio",
            ])
            .args(["--port", &ports[0].to_string()])
            .args(["--ws-port", &ports[1].to_string()])
            .args(["--studio-port", &ports[2].to_string()])
            .arg("--snapshot")
            .arg(snapshot_path)
            .current_dir(directory.path())
            .env_clear()
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Self {
            child,
            directory,
            ports,
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.directory.path().join("surfpool.log")).unwrap()
    }

    fn wait_for_phase(&mut self, fully_started: bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "exited before readiness: {}",
                self.log()
            );
            let ready = TcpStream::connect(("127.0.0.1", self.ports[2])).and_then(|mut stream| {
                stream.set_read_timeout(Some(Duration::from_millis(100)))?;
                stream.write_all(
                    b"GET /config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )?;
                let mut response = [0; 128];
                let size = stream.read(&mut response)?;
                Ok(String::from_utf8_lossy(&response[..size]).contains("200 OK"))
            });
            let logging_started = self.log().contains("Genesis airdrop successful");
            match ready {
                Ok(true) if !fully_started || logging_started => {
                    assert_eq!(
                        logging_started, fully_started,
                        "missed the early-startup window"
                    );
                    return;
                }
                Err(error) if Instant::now() >= deadline => {
                    panic!("Studio readiness failed: {error}; {}", self.log());
                }
                _ => {}
            }
            assert!(
                Instant::now() < deadline,
                "readiness timeout: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn stop(&mut self, signal: &str) {
        let result = Command::new("kill")
            .args([signal, &self.child.id().to_string()])
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "shutdown timeout: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "{status}: {}", self.log());
        for port in self.ports {
            match TcpStream::connect(("127.0.0.1", port)) {
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused),
                Ok(_) => panic!("port {port} still listening after exit"),
            }
        }
    }
}

impl Drop for Surfpool {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            result => {
                eprintln!("Cleaning up unfinished test child: {result:?}");
                if let Err(error) = self.child.kill() {
                    eprintln!("Test child cleanup kill failed: {error}");
                }
                if let Err(error) = self.child.wait() {
                    eprintln!("Test child cleanup wait failed: {error}");
                }
            }
        }
    }
}

fn snapshot_key(index: u32) -> String {
    let mut bytes = [0; 32];
    bytes[28..].copy_from_slice(&index.to_be_bytes());
    solana_pubkey::Pubkey::new_from_array(bytes).to_string()
}

#[tokio::test]
async fn startup_drains_snapshot_events_before_the_bounded_queue_fills() {
    let count = 1025;
    let mut surfpool = Surfpool::with_snapshot(count, String::new());
    surfpool.wait_for_phase(true);
    let response: serde_json::Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}", surfpool.ports[0]))
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getBalance",
            "params": [snapshot_key(count), {"commitment": "processed"}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["result"]["value"], 1_000_000, "{response}");
    surfpool.stop("-TERM");
}

#[test]
fn signals_stop_all_servers_during_startup_and_after_readiness() {
    for signal in ["-TERM", "-INT"] {
        for fully_started in [false, false, false, true] {
            let mut surfpool = Surfpool::start();
            surfpool.wait_for_phase(fully_started);
            surfpool.stop(signal);
        }
    }
}
