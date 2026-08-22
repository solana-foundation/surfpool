//! Process-wide counters for storage resource accounting.
//!
//! `lsof` and `ps` sample the process from outside; these counters record the
//! storage layer's own resource events, so a workload's before/after delta is
//! exact and assertable in tests. SQLite only: the PostgreSQL pool is
//! process-shared by design and its sessions live on the server.
//!
//! Counting is always compiled in: the counters are a handful of relaxed
//! atomics bumped at pool and connection lifecycle events, which are rare
//! next to queries. Each event also logs at debug level under this module's
//! target, so any build reports its storage resource movement when run with
//! `RUST_LOG=surfpool_core::storage::census=debug`.

use std::sync::atomic::{AtomicU64, Ordering};

use log::debug;

static POOLS_CREATED: AtomicU64 = AtomicU64::new(0);
static POOLS_DROPPED: AtomicU64 = AtomicU64::new(0);
static POOLS_PEAK: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_OPENED: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_CLOSED: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_PEAK: AtomicU64 = AtomicU64::new(0);

fn pool_created() {
    POOLS_CREATED.fetch_add(1, Ordering::Relaxed);
    let live = live_pools();
    POOLS_PEAK.fetch_max(live, Ordering::Relaxed);
    debug!("pool created (live: {})", live);
}

fn pool_dropped() {
    POOLS_DROPPED.fetch_add(1, Ordering::Relaxed);
    debug!("pool dropped (live: {})", live_pools());
}

fn live_pools() -> u64 {
    POOLS_CREATED
        .load(Ordering::Relaxed)
        .saturating_sub(POOLS_DROPPED.load(Ordering::Relaxed))
}

pub fn connection_opened() {
    CONNECTIONS_OPENED.fetch_add(1, Ordering::Relaxed);
    let live = live_connections();
    CONNECTIONS_PEAK.fetch_max(live, Ordering::Relaxed);
    debug!("connection opened (live: {})", live);
}

pub fn connection_closed() {
    CONNECTIONS_CLOSED.fetch_add(1, Ordering::Relaxed);
    debug!("connection closed (live: {})", live_connections());
}

fn live_connections() -> u64 {
    CONNECTIONS_OPENED
        .load(Ordering::Relaxed)
        .saturating_sub(CONNECTIONS_CLOSED.load(Ordering::Relaxed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CensusSnapshot {
    pub pools_created: u64,
    pub pools_dropped: u64,
    /// Pools created and not yet dropped at snapshot time.
    pub pools_live: u64,
    /// High-water mark of live pools over the process lifetime.
    pub pools_peak: u64,
    pub connections_opened: u64,
    pub connections_closed: u64,
    /// Connections opened and not yet closed at snapshot time.
    pub connections_live: u64,
    /// High-water mark of live connections over the process lifetime.
    pub connections_peak: u64,
}

pub fn snapshot() -> CensusSnapshot {
    CensusSnapshot {
        pools_created: POOLS_CREATED.load(Ordering::Relaxed),
        pools_dropped: POOLS_DROPPED.load(Ordering::Relaxed),
        pools_live: live_pools(),
        pools_peak: POOLS_PEAK.load(Ordering::Relaxed),
        connections_opened: CONNECTIONS_OPENED.load(Ordering::Relaxed),
        connections_closed: CONNECTIONS_CLOSED.load(Ordering::Relaxed),
        connections_live: live_connections(),
        connections_peak: CONNECTIONS_PEAK.load(Ordering::Relaxed),
    }
}

impl CensusSnapshot {
    /// The counter movement between `earlier` and `self`. Monotonic fields
    /// subtract; the `_live` fields are the live counts at `self`, and the
    /// `_peak` fields are process-lifetime high-water marks.
    pub fn since(&self, earlier: &CensusSnapshot) -> CensusSnapshot {
        CensusSnapshot {
            pools_created: self.pools_created - earlier.pools_created,
            pools_dropped: self.pools_dropped - earlier.pools_dropped,
            pools_live: self.pools_live,
            pools_peak: self.pools_peak,
            connections_opened: self.connections_opened - earlier.connections_opened,
            connections_closed: self.connections_closed - earlier.connections_closed,
            connections_live: self.connections_live,
            connections_peak: self.connections_peak,
        }
    }
}

/// A SQLite connection that reports its own close: r2d2's
/// `CustomizeConnection::on_release` fires when the pool discards a broken or
/// reaped connection, not at pool drop, so `Drop` on the connection itself is
/// the only signal that counts every close.
#[cfg(feature = "sqlite")]
pub struct CountedConnection(surfpool_db::diesel::SqliteConnection);

#[cfg(feature = "sqlite")]
impl Drop for CountedConnection {
    fn drop(&mut self) {
        connection_closed();
    }
}

#[cfg(feature = "sqlite")]
impl std::ops::Deref for CountedConnection {
    type Target = surfpool_db::diesel::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(feature = "sqlite")]
impl std::ops::DerefMut for CountedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// A connection manager that doubles as the pool's lifetime marker: r2d2
/// holds exactly one manager per pool, inside the shared state every pool
/// handle clones, so the manager drops when the last handle drops. Counting
/// pools here is balanced by construction; a pool whose build fails counts
/// one create and one drop.
#[cfg(feature = "sqlite")]
pub struct CountingSqliteManager(
    surfpool_db::diesel::r2d2::ConnectionManager<surfpool_db::diesel::SqliteConnection>,
);

#[cfg(feature = "sqlite")]
impl CountingSqliteManager {
    pub fn new(connection_string: &str) -> Self {
        pool_created();
        Self(surfpool_db::diesel::r2d2::ConnectionManager::new(
            connection_string,
        ))
    }
}

#[cfg(feature = "sqlite")]
impl Drop for CountingSqliteManager {
    fn drop(&mut self) {
        pool_dropped();
    }
}

#[cfg(feature = "sqlite")]
impl surfpool_db::diesel::r2d2::ManageConnection for CountingSqliteManager {
    type Connection = CountedConnection;
    type Error = surfpool_db::diesel::r2d2::Error;

    fn connect(&self) -> Result<Self::Connection, Self::Error> {
        let conn = self.0.connect()?;
        connection_opened();
        Ok(CountedConnection(conn))
    }

    fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        self.0.is_valid(&mut conn.0)
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        self.0.has_broken(&mut conn.0)
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;

    /// Dropping the backend (and its stores) must close every connection it
    /// opened.
    #[test]
    #[ignore = "process-global counters; run alone with --ignored --nocapture"]
    fn dropping_backend_closes_connections() {
        let _ = env_logger::builder().is_test(true).try_init();
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        let before = snapshot();
        let backend = crate::storage::sqlite::SqliteBackend::open(db_path, "census").unwrap();
        let store: crate::storage::SqliteStorage<String, String> =
            backend.open_store("census_table").unwrap();
        let opened = snapshot().since(&before);
        assert!(
            opened.connections_opened > 0,
            "pool should open connections"
        );

        drop(store);
        drop(backend);
        // A background job opening a connection toward min_idle holds the
        // pool alive until it finishes, so the closes can land a moment
        // after the drop; wait for the accounting to balance.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut after = snapshot().since(&before);
        while after.connections_closed != after.connections_opened
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
            after = snapshot().since(&before);
        }
        assert_eq!(
            after.connections_closed, after.connections_opened,
            "every connection opened by the backend should close at drop"
        );
        assert_eq!(
            after.pools_dropped, after.pools_created,
            "the backend's pool should drop with it"
        );
    }

    /// The per-surfnet resource cost, measured three ways: surfnets built and
    /// dropped one at a time (steady-state cost of one), N surfnets held live
    /// at once (the cost `cargo test` parallelism pays), and the drop of all
    /// N (everything released). Prints the exact counter movement per phase.
    #[test]
    #[ignore = "process-global counters; run alone with --ignored --nocapture"]
    fn census_workload() {
        const N: usize = 10;

        let _ = env_logger::builder().is_test(true).try_init();

        let t0 = snapshot();
        for _ in 0..N {
            let tt = crate::storage::tests::TestType::sqlite();
            let (svm, _simnet_rx, _geyser_rx) = tt.initialize_svm();
            drop(svm);
        }
        let t1 = snapshot();
        println!("sequential on-disk x{}: {:?}", N, t1.since(&t0));

        let held: Vec<_> = (0..N)
            .map(|_| crate::storage::tests::TestType::sqlite().initialize_svm())
            .collect();
        let t2 = snapshot();
        println!("held on-disk x{}:      {:?}", N, t2.since(&t1));

        drop(held);
        let t3 = snapshot();
        println!("after dropping all {}: {:?}", N, t3.since(&t2));
    }
}
