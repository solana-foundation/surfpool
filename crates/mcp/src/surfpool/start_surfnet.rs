use std::{process::Command, time::Duration};

use serde::Serialize;
use surfpool_core::{start_local_surfnet, surfnet::svm::SurfnetSvm};
use surfpool_types::{SimnetConfig, SimnetEvent, SurfpoolConfig};

#[derive(Serialize)]
pub struct StartSurfnetResponse {
    pub success: Option<StartSurfnetSuccess>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct StartSurfnetSuccess {
    pub kind: StartSurfnetKind,
    pub surfnet_url: String,
    pub surfnet_id: u16,
}

#[derive(Serialize)]
pub enum StartSurfnetKind {
    Command(SerializeCommand),
    Headless,
}

#[derive(Serialize)]
pub struct SerializeCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl From<Command> for SerializeCommand {
    fn from(command: Command) -> Self {
        Self {
            program: command.get_program().to_string_lossy().to_string(),
            args: command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect(),
        }
    }
}
impl StartSurfnetResponse {
    pub fn success(data: StartSurfnetSuccess) -> Self {
        Self {
            success: Some(data),
            error: None,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            success: None,
            error: Some(message),
        }
    }
}

pub fn generate_command(rpc_port: u16, ws_port: u16) -> Command {
    let mut cmd = Command::new("surfpool");
    cmd.arg("start");
    cmd.arg("--port").arg(format!("{}", rpc_port));
    cmd.arg("--ws-port").arg(format!("{}", ws_port));
    cmd.arg("--no-deploy");
    cmd
}

pub fn run_command(surfnet_id: u16, rpc_port: u16, ws_port: u16) -> StartSurfnetResponse {
    let command = generate_command(rpc_port, ws_port);
    let surfnet_url = format!("http://127.0.0.1:{}", rpc_port);

    StartSurfnetResponse::success(StartSurfnetSuccess {
        kind: StartSurfnetKind::Command(command.into()),
        surfnet_url,
        surfnet_id,
    })
}

pub fn run_headless(surfnet_id: u16, rpc_port: u16, ws_port: u16) -> StartSurfnetResponse {
    let (surfnet_svm, simnet_events_rx, geyser_events_rx) = SurfnetSvm::default();

    let (simnet_commands_tx, simnet_commands_rx) = crossbeam_channel::unbounded();

    let simnet_events_tx = surfnet_svm.simnet_events_tx.clone();

    // The default StartupPlanner::Runloop makes the runloop seal an empty
    // startup plan before announcing Ready, so this embedded surfnet reads
    // as publicly ready without any sealing choreography here.
    let mut config = SurfpoolConfig::default();
    config.rpc.bind_port = rpc_port;
    config.rpc.ws_port = ws_port;

    let simnet_config = SimnetConfig {
        expiry: Some(15 * 60 * 1000),
        ..Default::default()
    };

    config.simnets = vec![simnet_config];

    let handle = hiro_system_kit::thread_named("surfnet").spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let future = start_local_surfnet(
                surfnet_svm,
                config,
                simnet_commands_tx,
                simnet_commands_rx,
                geyser_events_rx,
            );
            hiro_system_kit::nestable_block_on(future)
        }));

        match result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                simnet_events_tx.error(format!("Surfnet operational error: {}", e));
            }
            Err(panic_payload) => {
                let panic_msg = match panic_payload.downcast_ref::<&'static str>() {
                    Some(s) => *s,
                    None => match panic_payload.downcast_ref::<String>() {
                        Some(s) => s.as_str(),
                        None => "Surfnet thread panicked with an unknown payload",
                    },
                };
                simnet_events_tx.error(format!("Surfnet thread panic: {}", panic_msg));
            }
        }
        Ok::<(), String>(())
    });

    let res = match handle {
        Ok(_) => loop {
            match simnet_events_rx.recv_timeout(Duration::from_secs(25)) {
                Ok(received_event) => match received_event {
                    SimnetEvent::Aborted(error) => {
                        return StartSurfnetResponse::error(error);
                    }
                    SimnetEvent::CoreStarted(_) => {
                        let surfnet_url = format!("http://127.0.0.1:{}", rpc_port);
                        break StartSurfnetResponse::success(StartSurfnetSuccess {
                            kind: StartSurfnetKind::Headless,
                            surfnet_url,
                            surfnet_id,
                        });
                    }
                    SimnetEvent::ErrorLog(_, error) => {
                        return StartSurfnetResponse::error(error);
                    }
                    _other_simnet_event => continue,
                },
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return StartSurfnetResponse::error(
                        "Surfnet initialization timed out waiting for an event.".to_string(),
                    );
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return StartSurfnetResponse::error(
                        "Surfnet channel disconnected while waiting for event.".to_string(),
                    );
                }
            }
        },
        Err(e) => StartSurfnetResponse::error(format!("Failed to spawn surfnet thread: {}", e)),
    };

    let _handle = hiro_system_kit::thread_named("surfnet-termination-handler").spawn(move || {
        loop {
            match simnet_events_rx.recv() {
                Ok(received_event) => match received_event {
                    SimnetEvent::Aborted(reason) => {
                        eprintln!("Surfnet instance terminated: {}", reason);

                        break;
                    }
                    SimnetEvent::Shutdown => {
                        eprintln!("Surfnet instance has shut down.");
                        break;
                    }
                    _ => {}
                },
                Err(e) => {
                    eprintln!(
                        "Error receiving simnet event in termination handler: {:?}",
                        e
                    );
                    break;
                }
            }
        }
    });

    res
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        time::{Duration, Instant},
    };

    use super::*;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Minimal JSON-RPC POST over a raw socket: HTTP/1.0 so the response is
    /// unchunked and terminated by connection close.
    fn get_surfnet_info(rpc_port: u16) -> serde_json::Value {
        let address = format!("127.0.0.1:{rpc_port}");
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"surfnet_getSurfnetInfo"}"#;
        let request = format!(
            "POST / HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let attempt = TcpStream::connect(&address).and_then(|mut stream| {
                stream.write_all(request.as_bytes())?;
                let mut response = String::new();
                stream.read_to_string(&mut response)?;
                Ok(response)
            });
            match attempt {
                Ok(response) => {
                    let json_start =
                        response.find("\r\n\r\n").expect("malformed HTTP response") + 4;
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

    // run_headless used to leave the startup plan unsealed, which projected
    // a forever-pending surfpool-startup execution into getSurfnetInfo and
    // starved readiness-checking clients (legacy Anchor's readiness loop has
    // no timeout). This pins the empty-plan seal in place.
    #[test]
    fn headless_surfnets_report_a_ready_startup() {
        let rpc_port = free_port();
        let ws_port = free_port();

        let response = run_headless(1, rpc_port, ws_port);
        assert!(
            response.error.is_none(),
            "surfnet failed to start: {:?}",
            response.error
        );

        let info = get_surfnet_info(rpc_port);
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
    }
}
