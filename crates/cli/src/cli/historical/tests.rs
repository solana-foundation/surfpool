use super::*;

#[test]
fn security_historical_airdrops_are_explicit_and_warn_about_signing_risk() {
    let cmd = StartSimnet::try_parse_from(["start", "--after", "1"]).unwrap();
    assert!(
        cmd.accounts.airdrop_keypair_path.is_empty(),
        "implicit path must not look explicit"
    );
    let (addresses, events) = cmd.get_airdrop_addresses();
    assert!(addresses.is_empty());
    let warning = format!("{events:?}");
    for risk in [
        "recent blockhash",
        "durable nonce",
        "disposable",
        "domain separation",
    ] {
        assert!(warning.contains(risk), "missing {risk}: {warning}");
    }
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::new();
    let path = dir.path().join("disposable.json");
    key.write_to_file(&path).unwrap();
    for historical in [false, true] {
        let mut argv = vec![
            "start",
            "--airdrop",
            "11111111111111111111111111111111",
            "-k",
            path.to_str().unwrap(),
        ];
        if historical {
            argv.extend(["--after", "1"]);
        }
        let cmd = StartSimnet::try_parse_from(argv).unwrap();
        assert_eq!(
            cmd.get_airdrop_addresses().0,
            vec![Pubkey::default(), key.pubkey()]
        );
    }
    let normal = StartSimnet::try_parse_from(["start"]).unwrap();
    assert!(!format!("{:?}", normal.get_airdrop_addresses().1).contains("domain separation"));
}

#[test]
fn interfaces_learn_what_a_historical_surfnet_forks_after_instead_of_a_datasource() {
    let historical = StartSimnet::try_parse_from(["start", "--after", "413023260"]).unwrap();
    assert_eq!(
        interface_sources(&historical, Some("https://tip.example".to_string())),
        (
            None,
            Some(SanitizedHistoricalConfig {
                after: "413023260".to_string()
            })
        )
    );
    let normal = StartSimnet::try_parse_from(["start"]).unwrap();
    assert_eq!(
        interface_sources(&normal, Some("https://datasource.example".to_string())),
        (Some("https://datasource.example".to_string()), None)
    );
}

fn args(cmd: &StartSimnet) -> Vec<String> {
    config(cmd).unwrap().unwrap().args
}

#[test]
fn historical_bare_selectors_are_forwarded_to_the_shared_native_parser() {
    for selector in [
        "413023260",
        "FohHkuSbQXEiDDhQCjq4HfC6kpBAeQBVh3Nb3v9bK4Dz7fgjtMnxUUvpfUsSbHQRGceTMiVJupCCNzW8LUWcPsp",
    ] {
        let cmd = StartSimnet::try_parse_from(["start", "--after", selector]).unwrap();
        assert!(
            args(&cmd)
                .windows(2)
                .any(|pair| pair == ["--after", selector])
        );
    }
}

#[test]
fn historical_runtime_registry_requires_and_forwards_its_independent_pin() {
    let cmd = StartSimnet::try_parse_from([
        "start",
        "--after",
        "tx:signature",
        "--historical-runtime-registry",
        "registry.json",
        "--historical-runtime-registry-sha256",
        "pin",
    ])
    .unwrap();
    let args = args(&cmd);
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--runtime-registry", "registry.json"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--runtime-registry-sha256", "pin"])
    );
    assert!(
        StartSimnet::try_parse_from([
            "start",
            "--after",
            "tx:signature",
            "--historical-runtime-registry",
            "registry.json"
        ])
        .is_err()
    );
}

#[test]
fn the_backend_is_private_and_cli_credentials_require_references() {
    let cmd = StartSimnet::try_parse_from([
        "start",
        "--after",
        "tx:signature",
        "--port",
        "9991",
        "--key",
        "synthetic-not-a-real-key",
    ])
    .unwrap();
    let error = config(&cmd).unwrap_err();
    assert!(error.contains("--key env:NAME"), "{error}");
    assert!(!error.contains("synthetic-not-a-real-key"), "{error}");
    let cmd = StartSimnet::try_parse_from([
        "start",
        "--after",
        "tx:signature",
        "--port",
        "9991",
        "--key",
        "env:MY_KEY",
    ])
    .unwrap();
    let config = config(&cmd).unwrap().unwrap();
    assert_eq!(
        &config.args[..6],
        &[
            "session",
            "--after",
            "tx:signature",
            "--backend",
            "stdio",
            "--data-dir"
        ]
    );
    // Surfpool serves RPC itself; the backend binds no port.
    assert!(
        !config
            .args
            .iter()
            .any(|arg| arg == "--port" || arg == "9991")
    );
    let rendered = format!(
        "{config:?} {cmd:?} {}",
        serde_json::to_string(&config).unwrap()
    );
    assert!(!rendered.contains("synthetic-not-a-real-key"), "{rendered}");
    assert!(
        config
            .args
            .windows(2)
            .any(|pair| pair == ["--key", "env:MY_KEY"])
    );
    assert!(config.envs.is_empty());

    let reference =
        StartSimnet::try_parse_from(["start", "--after", "1", "--key", "env:MY_KEY"]).unwrap();
    let config = super::config(&reference).unwrap().unwrap();
    assert!(
        config
            .args
            .windows(2)
            .any(|pair| pair == ["--key", "env:MY_KEY"])
    );
    assert!(config.envs.is_empty());
    let from_file =
        StartSimnet::try_parse_from(["start", "--after", "1", "--key-file", "provider-key.txt"])
            .unwrap();
    assert!(
        args(&from_file)
            .windows(2)
            .any(|pair| pair == ["--key-file", "provider-key.txt"])
    );
}

#[test]
fn ordinary_surfpool_options_stay_available_and_current_chain_options_conflict() {
    for option in [
        vec!["--daemon"],
        vec!["--snapshot", "accounts.json"],
        vec!["--watch"],
        vec!["--slot-time", "100"],
        vec!["--airdrop", "11111111111111111111111111111111"],
        vec!["--no-tui"],
    ] {
        let mut argv = vec!["start", "--after", "1"];
        argv.extend(option.iter().copied());
        assert!(StartSimnet::try_parse_from(&argv).is_ok(), "{option:?}");
    }
    for option in [
        vec!["--rpc-url", "https://example.invalid"],
        vec!["--offline"],
        vec!["--skip-signature-verification"],
        // Historical slots stay frozen until advanced explicitly (resumeClock, timeTravel).
        vec!["--block-production-mode", "transaction"],
    ] {
        let mut argv = vec!["start", "--after", "1"];
        argv.extend(option.iter().copied());
        assert!(StartSimnet::try_parse_from(&argv).is_err(), "{option:?}");
    }
    let normal = StartSimnet::try_parse_from(["start"]).unwrap();
    assert!(config(&normal).unwrap().is_none());
}

#[test]
fn a_persistent_store_journals_by_a_binding_that_holds_no_credential() {
    let journal = |db: &str, id: &str| {
        let cmd =
            StartSimnet::try_parse_from(["start", "--after", "1", "--db", db, "--surfnet-id", id])
                .unwrap();
        let args = args(&cmd);
        let value = |flag: &str| {
            let at = args.iter().position(|arg| arg == flag)?;
            args.get(at + 1).cloned()
        };
        (value("--journal"), value("--journal-binding"))
    };
    assert_eq!(journal(":memory:", "a"), (None, None), "nothing persists");
    let (path, binding) = journal("store.sqlite", "a");
    let binding = binding.unwrap();
    assert_eq!(binding.len(), 64);
    assert!(
        path.unwrap()
            .ends_with(&format!("journals/{binding}.jsonl"))
    );
    assert_ne!(
        journal("store.sqlite", "b").1.unwrap(),
        binding,
        "per surfnet"
    );
    assert_ne!(
        journal("other.sqlite", "a").1.unwrap(),
        binding,
        "per store"
    );
    // A database URL's password and parameters reach neither the arguments nor the binding.
    let url = "postgresql://surfpool:s3cret@127.0.0.1:5432/surfnet?password=s3cret";
    let (path, secret) = journal(url, "a");
    assert!(!path.unwrap().contains("s3cret"));
    let plain = journal("postgresql://surfpool@127.0.0.1:5432/surfnet", "a").1;
    assert_eq!(
        Some(secret.unwrap()),
        plain,
        "the same database, without its password"
    );
    assert_eq!(
        database_identity("postgresql://u:p@w@h:1/db?sslmode=require"),
        "postgresql://u@h:1/db"
    );
}
