use std::{
    fs::{self, File},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn strict_snapshot_offline_missing_account_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let address = "11111111111111111111111111111111";
    fs::write(
        dir.path().join("snapshot.json"),
        format!("{{\"{address}\":null}}"),
    )
    .unwrap();

    // Hold all three sockets until ports have been selected so they are distinct.
    let listeners: Vec<_> = (0..3)
        .map(|_| TcpListener::bind(("127.0.0.1", 0)).unwrap())
        .collect();
    let ports: Vec<_> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    drop(listeners);
    let stdout = dir.path().join("stdout.txt");
    let stderr = dir.path().join("stderr.txt");
    // Files avoid a full output pipe blocking the child before it exits.
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_surfpool"))
            .args([
                "start",
                "--offline",
                "--no-deploy",
                "--no-tui",
                "--no-studio",
                "--airdrop-amount",
                "0",
                "--snapshot",
                "snapshot.json",
                "--strict-snapshot",
                "--port",
                &ports[0].to_string(),
                "--ws-port",
                &ports[1].to_string(),
                "--studio-port",
                &ports[2].to_string(),
            ])
            .current_dir(dir.path())
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .expect("start surfpool"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rpc_observed = false;
    let status = loop {
        rpc_observed |= TcpStream::connect(("127.0.0.1", ports[0])).is_ok();
        if let Some(status) = child.0.try_wait().expect("poll surfpool") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "surfpool did not exit within 30 seconds"
        );
        thread::sleep(Duration::from_millis(50));
    };
    let output = format!(
        "{}\n{}",
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
    assert!(!status.success(), "unexpected success: {output}");
    assert!(
        output.contains(address),
        "missing account address: {output}"
    );
    assert!(
        !output.contains("Preloaded"),
        "unexpected preload success: {output}"
    );
    assert!(
        !rpc_observed,
        "RPC became available before failure: {output}"
    );
}
