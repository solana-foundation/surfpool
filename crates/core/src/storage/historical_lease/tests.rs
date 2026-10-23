use super::HistoricalStoreLease;

#[test]
fn ephemeral_stores_need_no_lease() {
    assert!(HistoricalStoreLease::acquire(None, "a").unwrap().is_none());
    assert!(
        HistoricalStoreLease::acquire(Some(":memory:"), "a")
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn normal_memory_uri_keeps_its_existing_ephemeral_behavior() {
    for uri in [
        "file::memory:",
        "file:normal-memory?mode=memory&cache=shared",
    ] {
        assert!(
            HistoricalStoreLease::normal(Some(uri), "a")
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_writer_is_exclusive_per_namespace_and_reusable_after_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite");
    let db = path.to_str().unwrap();
    let writer = HistoricalStoreLease::acquire(Some(db), "a").unwrap();
    let second = HistoricalStoreLease::acquire(Some(db), "a");
    assert!(second.err().unwrap().starts_with("SESSION_IN_USE:"));
    let other = HistoricalStoreLease::acquire(Some(db), "b").unwrap();
    drop(writer);
    assert!(
        HistoricalStoreLease::acquire(Some(db), "a")
            .unwrap()
            .is_some()
    );
    drop(other);
    let normal = HistoricalStoreLease::normal(Some(db), "a").unwrap();
    let sibling = HistoricalStoreLease::normal(Some(db), "a").unwrap();
    assert!(
        HistoricalStoreLease::acquire(Some(db), "a")
            .err()
            .unwrap()
            .starts_with("SESSION_IN_USE:")
    );
    drop(normal);
    drop(sibling);
    let writer = HistoricalStoreLease::acquire(Some(db), "a").unwrap();
    assert!(
        HistoricalStoreLease::normal(Some(db), "a")
            .err()
            .unwrap()
            .starts_with("SESSION_IN_USE:")
    );
    drop(writer);
}

#[cfg(all(feature = "sqlite", unix))]
#[test]
fn sqlite_symlink_and_file_uri_aliases_cannot_bypass_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state with spaces.sqlite");
    let writer = HistoricalStoreLease::acquire(Some(path.to_str().unwrap()), "a").unwrap();
    let alias = directory.path().join("alias.sqlite");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    for db in [
        alias.to_str().unwrap().to_string(),
        format!("file:{}", path.display()),
        format!("file:{}", path.display().to_string().replace(' ', "%20")),
    ] {
        assert!(
            HistoricalStoreLease::acquire(Some(&db), "a")
                .err()
                .unwrap()
                .starts_with("SESSION_IN_USE:")
        );
    }
    drop(writer);
}

#[cfg(feature = "postgres")]
#[test]
#[ignore = "requires an explicitly provided disposable PostgreSQL server"]
fn postgres_writer_is_exclusive_across_connections_and_released_on_drop() {
    let url = std::env::var("SURFPOOL_TEST_POSTGRES_URL").unwrap();
    let namespace = format!("historical-lease-{}", std::process::id());
    let writer = HistoricalStoreLease::acquire(Some(&url), &namespace).unwrap();
    assert!(
        HistoricalStoreLease::acquire(Some(&url), &namespace)
            .err()
            .unwrap()
            .starts_with("SESSION_IN_USE:")
    );
    let other = HistoricalStoreLease::acquire(Some(&url), &(namespace.clone() + "-other")).unwrap();
    drop(writer);
    assert!(
        HistoricalStoreLease::acquire(Some(&url), &namespace)
            .unwrap()
            .is_some()
    );
    drop(other);
    let normal = HistoricalStoreLease::normal(Some(&url), &namespace).unwrap();
    let sibling = HistoricalStoreLease::normal(Some(&url), &namespace).unwrap();
    assert!(
        HistoricalStoreLease::acquire(Some(&url), &namespace)
            .err()
            .unwrap()
            .starts_with("SESSION_IN_USE:")
    );
    drop(normal);
    drop(sibling);
    let writer = HistoricalStoreLease::acquire(Some(&url), &namespace).unwrap();
    assert!(
        HistoricalStoreLease::normal(Some(&url), &namespace)
            .err()
            .unwrap()
            .starts_with("SESSION_IN_USE:")
    );
    drop(writer);
}
