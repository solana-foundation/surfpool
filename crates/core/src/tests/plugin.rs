use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use agave_geyser_plugin_interface::geyser_plugin_interface::GeyserPlugin;
use crossbeam_channel::unbounded;
use libloading::Library;
use tempfile::NamedTempFile;

use crate::{
    PluginInfo,
    runloops::{ManagedPlugin, load_plugin_from_config, unload_plugin_by_name},
    surfnet::PluginCommand,
};

/// Path to the pre-built hello-geyser plugin dylib, relative to workspace root.
const HELLO_GEYSER_CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/hello-geyser/hello-geyser.json"
);

fn hello_geyser_dylib_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!(
        "{}/../../examples/hello-geyser/target/release/libhello_geyser.dylib",
        manifest_dir
    )
}

/// Helper to write a temporary plugin config file pointing at the hello-geyser dylib.
fn write_plugin_config(libpath: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("failed to create temp file");
    write!(
        f,
        r#"{{ "name": "test-plugin", "libpath": "{}" }}"#,
        libpath
    )
    .unwrap();
    f.flush().unwrap();
    f
}

// ── Error path tests (no dylib needed) ──────────────────────────────────

#[test]
fn test_load_plugin_missing_config_file() {
    let result = load_plugin_from_config("/nonexistent/path/plugin.json", false);
    let err = result.err().expect("expected error");
    assert!(
        err.contains("Unable to read plugin config"),
        "expected config read error, got: {err}"
    );
}

#[test]
fn test_load_plugin_invalid_json() {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, "not valid json {{{{").unwrap();
    f.flush().unwrap();

    let result = load_plugin_from_config(f.path().to_str().unwrap(), false);
    let err = result.err().expect("expected error");
    assert!(
        err.contains("Unable to parse plugin manifest"),
        "expected JSON parse error, got: {err}"
    );
}

#[test]
fn test_load_plugin_missing_libpath() {
    let mut f = NamedTempFile::new().unwrap();
    write!(f, r#"{{ "name": "test" }}"#).unwrap();
    f.flush().unwrap();

    let result = load_plugin_from_config(f.path().to_str().unwrap(), false);
    let err = result.err().expect("expected error");
    assert!(
        err.contains("libpath"),
        "expected missing libpath error, got: {err}"
    );
}

#[test]
fn test_load_plugin_nonexistent_dylib() {
    let config = write_plugin_config("/nonexistent/libfake.dylib");

    let result = load_plugin_from_config(config.path().to_str().unwrap(), false);
    let err = result.err().expect("expected error");
    assert!(
        err.contains("Unable to load plugin"),
        "expected dylib load error, got: {err}"
    );
}

#[test]
fn test_unload_plugin_not_found() {
    let mut plugins: Vec<ManagedPlugin> = vec![];
    let result = unload_plugin_by_name("nonexistent", &mut plugins);
    let err = result.err().expect("expected error");
    assert!(
        err.contains("not found"),
        "expected not found error, got: {err}"
    );
}

// ── Teardown tests (in-process plugin, no dylib needed) ─────────────────

/// Plugin that records its `on_unload` call and its own destruction, so tests
/// can check teardown order.
#[derive(Debug)]
struct RecordingPlugin {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl GeyserPlugin for RecordingPlugin {
    fn name(&self) -> &'static str {
        "recording-plugin"
    }

    fn on_unload(&mut self) {
        self.events.lock().unwrap().push("on_unload");
    }
}

impl Drop for RecordingPlugin {
    fn drop(&mut self) {
        self.events.lock().unwrap().push("drop");
    }
}

/// Returns a handle to the running test binary, standing in for a plugin
/// library since `RecordingPlugin` is compiled into the host.
fn current_process_library() -> Library {
    #[cfg(unix)]
    let library = libloading::os::unix::Library::this();
    #[cfg(windows)]
    let library = libloading::os::windows::Library::this().unwrap();
    library.into()
}

fn recording_managed_plugin() -> (ManagedPlugin, Arc<Mutex<Vec<&'static str>>>) {
    let events = Arc::new(Mutex::new(vec![]));
    let plugin = RecordingPlugin {
        events: events.clone(),
    };
    let managed = ManagedPlugin {
        uuid: crate::Uuid::new_v4(),
        name: plugin.name().to_string(),
        plugin: Box::new(plugin),
        _library: current_process_library(),
        config_path: "recording-plugin.json".to_string(),
    };
    (managed, events)
}

#[test]
fn test_dropping_loaded_plugins_calls_on_unload_once() {
    // The geyser runloop drops its plugin list when it exits; plugins must still
    // get `on_unload` so they can stop workers before their library unloads.
    let (managed, events) = recording_managed_plugin();
    let managed_plugins = vec![managed];

    drop(managed_plugins);

    assert_eq!(*events.lock().unwrap(), vec!["on_unload", "drop"]);
}

#[test]
fn test_unload_plugin_by_name_calls_on_unload_once() {
    let (managed, events) = recording_managed_plugin();
    let mut managed_plugins = vec![managed];

    unload_plugin_by_name("recording-plugin", &mut managed_plugins).unwrap();

    assert!(managed_plugins.is_empty());
    assert_eq!(*events.lock().unwrap(), vec!["on_unload", "drop"]);
}

// ── Happy path tests (require pre-built hello-geyser dylib) ─────────────

fn skip_if_no_dylib() -> bool {
    !std::path::Path::new(&hello_geyser_dylib_path()).exists()
}

#[test]
fn test_load_plugin_success() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    let config = write_plugin_config(&hello_geyser_dylib_path());
    let result = load_plugin_from_config(config.path().to_str().unwrap(), false);
    assert!(
        result.is_ok(),
        "load_plugin_from_config failed: {:?}",
        result.err()
    );

    let managed = result.unwrap();
    assert_eq!(managed.name, "hello-geyser");
    assert!(!managed.uuid.is_nil());
    assert_eq!(managed.config_path, config.path().to_str().unwrap());
}

#[test]
fn test_load_and_unload_plugin() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    let config = write_plugin_config(&hello_geyser_dylib_path());
    let managed = load_plugin_from_config(config.path().to_str().unwrap(), false).unwrap();
    let plugin_name = managed.name.clone();

    let mut plugins = vec![managed];
    assert_eq!(plugins.len(), 1);

    let result = unload_plugin_by_name(&plugin_name, &mut plugins);
    assert!(result.is_ok());
    assert!(plugins.is_empty());
}

#[test]
fn test_unload_preserves_other_plugins() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    // Load two instances with different configs (they'll both be named "hello-geyser"
    // but we can test that only the first match is removed)
    let config1 = write_plugin_config(&hello_geyser_dylib_path());
    let managed1 = load_plugin_from_config(config1.path().to_str().unwrap(), false).unwrap();

    let config2 = write_plugin_config(&hello_geyser_dylib_path());
    let managed2 = load_plugin_from_config(config2.path().to_str().unwrap(), false).unwrap();
    let uuid2 = managed2.uuid;

    let mut plugins = vec![managed1, managed2];

    // Unload first match
    unload_plugin_by_name("hello-geyser", &mut plugins).unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        plugins[0].uuid, uuid2,
        "should have removed the first match"
    );
}

#[test]
fn test_load_plugin_with_reload_flag() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    let config = write_plugin_config(&hello_geyser_dylib_path());
    let result = load_plugin_from_config(config.path().to_str().unwrap(), true);
    assert!(result.is_ok(), "reload load failed: {:?}", result.err());
}

#[test]
fn test_load_plugin_from_existing_config() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    // Use the actual hello-geyser.json config that ships with the example
    let result = load_plugin_from_config(HELLO_GEYSER_CONFIG, false);
    assert!(
        result.is_ok(),
        "loading from example config failed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap().name, "hello-geyser");
}

// ── Channel-based tests (PluginCommand flow) ────────────────────────────

#[test]
fn test_plugin_command_list_empty() {
    // Simulate the geyser runloop side: recv command, respond
    let (cmd_tx, cmd_rx) = unbounded::<PluginCommand>();
    let (resp_tx, resp_rx) = crossbeam_channel::bounded::<Vec<PluginInfo>>(1);

    cmd_tx
        .send(PluginCommand::List {
            response_tx: resp_tx,
        })
        .unwrap();

    // Simulate geyser handler
    if let Ok(PluginCommand::List { response_tx }) = cmd_rx.recv() {
        let _ = response_tx.send(vec![]);
    }

    let result = resp_rx.recv().unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_plugin_command_load_and_list() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    let (cmd_tx, cmd_rx) = unbounded::<PluginCommand>();

    // Simulate geyser handler in a thread
    let handler = std::thread::spawn(move || {
        let mut managed_plugins: Vec<ManagedPlugin> = vec![];

        // Handle two commands: Load then List
        for _ in 0..2 {
            match cmd_rx.recv().unwrap() {
                PluginCommand::Load {
                    config_file,
                    response_tx,
                } => {
                    let result = match load_plugin_from_config(&config_file, false) {
                        Ok(managed) => {
                            let info = PluginInfo {
                                plugin_name: managed.name.clone(),
                                uuid: managed.uuid.to_string(),
                            };
                            managed_plugins.push(managed);
                            Ok(info)
                        }
                        Err(e) => Err(e),
                    };
                    let _ = response_tx.send(result);
                }
                PluginCommand::List { response_tx } => {
                    let infos = managed_plugins
                        .iter()
                        .map(|p| PluginInfo {
                            plugin_name: p.name.clone(),
                            uuid: p.uuid.to_string(),
                        })
                        .collect();
                    let _ = response_tx.send(infos);
                }
                _ => panic!("unexpected command"),
            }
        }
    });

    // Send Load command
    let config = write_plugin_config(&hello_geyser_dylib_path());
    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    cmd_tx
        .send(PluginCommand::Load {
            config_file: config.path().to_str().unwrap().to_string(),
            response_tx: resp_tx,
        })
        .unwrap();

    let load_result = resp_rx.recv().unwrap();
    assert!(load_result.is_ok());
    let plugin_info = load_result.unwrap();
    assert_eq!(plugin_info.plugin_name, "hello-geyser");

    // Send List command
    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    cmd_tx
        .send(PluginCommand::List {
            response_tx: resp_tx,
        })
        .unwrap();

    let list_result = resp_rx.recv().unwrap();
    assert_eq!(list_result.len(), 1);
    assert_eq!(list_result[0].plugin_name, "hello-geyser");
    assert_eq!(list_result[0].uuid, plugin_info.uuid);

    handler.join().unwrap();
}

#[test]
fn test_plugin_command_load_unload_list() {
    if skip_if_no_dylib() {
        eprintln!("Skipping: hello-geyser dylib not built");
        return;
    }

    let (cmd_tx, cmd_rx) = unbounded::<PluginCommand>();

    let handler = std::thread::spawn(move || {
        let mut managed_plugins: Vec<ManagedPlugin> = vec![];

        // Handle three commands: Load, Unload, List
        for _ in 0..3 {
            match cmd_rx.recv().unwrap() {
                PluginCommand::Load {
                    config_file,
                    response_tx,
                } => {
                    let result = match load_plugin_from_config(&config_file, false) {
                        Ok(managed) => {
                            let info = PluginInfo {
                                plugin_name: managed.name.clone(),
                                uuid: managed.uuid.to_string(),
                            };
                            managed_plugins.push(managed);
                            Ok(info)
                        }
                        Err(e) => Err(e),
                    };
                    let _ = response_tx.send(result);
                }
                PluginCommand::Unload { name, response_tx } => {
                    let result = unload_plugin_by_name(&name, &mut managed_plugins);
                    let _ = response_tx.send(result);
                }
                PluginCommand::List { response_tx } => {
                    let infos = managed_plugins
                        .iter()
                        .map(|p| PluginInfo {
                            plugin_name: p.name.clone(),
                            uuid: p.uuid.to_string(),
                        })
                        .collect();
                    let _ = response_tx.send(infos);
                }
                _ => panic!("unexpected command"),
            }
        }
    });

    // Load
    let config = write_plugin_config(&hello_geyser_dylib_path());
    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    cmd_tx
        .send(PluginCommand::Load {
            config_file: config.path().to_str().unwrap().to_string(),
            response_tx: resp_tx,
        })
        .unwrap();
    let load_result = resp_rx.recv().unwrap();
    assert!(load_result.is_ok());

    // Unload
    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    cmd_tx
        .send(PluginCommand::Unload {
            name: "hello-geyser".to_string(),
            response_tx: resp_tx,
        })
        .unwrap();
    let unload_result = resp_rx.recv().unwrap();
    assert!(unload_result.is_ok());

    // List — should be empty now
    let (resp_tx, resp_rx) = crossbeam_channel::bounded(1);
    cmd_tx
        .send(PluginCommand::List {
            response_tx: resp_tx,
        })
        .unwrap();
    let list_result = resp_rx.recv().unwrap();
    assert!(list_result.is_empty());

    handler.join().unwrap();
}
