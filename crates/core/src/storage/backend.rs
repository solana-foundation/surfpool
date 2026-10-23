use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::storage::{OverlayLike, Storage, StorageHashMap, StorageResult};

/// Per-surfnet storage backend. Owns the connection resources for one surfnet
/// (the SQLite pool, or a lease on the shared PostgreSQL pool) and mints the
/// per-table stores that share them. Constructed once per surfnet from the
/// database URL; dropping the surfnet drops the backend and its connections.
/// A persistent backend's stores also record whether any of their writes failed.
#[derive(Clone)]
pub enum StorageBackend {
    /// No database URL: stores are independent in-process maps.
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(super::sqlite::SqliteBackend, Arc<AtomicBool>),
    #[cfg(feature = "postgres")]
    Postgres(super::postgres::PostgresBackend, Arc<AtomicBool>),
}

/// A persistent store that records any failed write in its backend's shared flag.
struct Watched<K, V> {
    inner: Box<dyn Storage<K, V>>,
    failed: Arc<AtomicBool>,
}

impl<K, V> Watched<K, V> {
    fn noted<T>(&self, result: StorageResult<T>) -> StorageResult<T> {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
}

impl<K: 'static, V: 'static> Storage<K, V> for Watched<K, V> {
    fn store(&mut self, key: K, value: V) -> StorageResult<()> {
        let stored = self.inner.store(key, value);
        self.noted(stored)
    }
    fn clear(&mut self) -> StorageResult<()> {
        let cleared = self.inner.clear();
        self.noted(cleared)
    }
    fn get(&self, key: &K) -> StorageResult<Option<V>> {
        self.inner.get(key)
    }
    fn take(&mut self, key: &K) -> StorageResult<Option<V>> {
        let taken = self.inner.take(key);
        self.noted(taken)
    }
    fn keys(&self) -> StorageResult<Vec<K>> {
        self.inner.keys()
    }
    fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        self.inner.into_iter()
    }
    fn contains_key(&self, key: &K) -> StorageResult<bool> {
        self.inner.contains_key(key)
    }
    fn count(&self) -> StorageResult<u64> {
        self.inner.count()
    }
    fn shutdown(&self) {
        self.inner.shutdown()
    }
    fn clone_box(&self) -> Box<dyn Storage<K, V>> {
        Box::new(Watched {
            inner: self.inner.clone_box(),
            failed: self.failed.clone(),
        })
    }
    fn as_overlay(&self) -> Option<&dyn OverlayLike<K, V>> {
        self.inner.as_overlay()
    }
}

impl StorageBackend {
    /// Selects and connects the backend for `database_url`: `None` is
    /// in-memory, a `postgres://`/`postgresql://` URL is PostgreSQL, and
    /// anything else is treated as an SQLite path (including `:memory:`).
    pub fn open(database_url: &Option<&str>, surfnet_id: &str) -> StorageResult<Self> {
        let Some(url) = database_url else {
            return Ok(StorageBackend::Memory);
        };
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            #[cfg(feature = "postgres")]
            {
                let backend = super::postgres::PostgresBackend::open(url, surfnet_id)?;
                return Ok(StorageBackend::Postgres(backend, Arc::default()));
            }
            #[cfg(not(feature = "postgres"))]
            return Err(super::StorageError::PostgresNotEnabled);
        }
        #[cfg(feature = "sqlite")]
        {
            let backend = super::sqlite::SqliteBackend::open(url, surfnet_id)?;
            Ok(StorageBackend::Sqlite(backend, Arc::default()))
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = surfnet_id;
            Err(super::StorageError::SqliteNotEnabled)
        }
    }

    /// Opens the kv store for `table_name`, backed by a hash map when the
    /// backend is [`StorageBackend::Memory`].
    pub fn open_store<K, V>(&self, table_name: &str) -> StorageResult<Box<dyn Storage<K, V>>>
    where
        K: serde::Serialize
            + serde::de::DeserializeOwned
            + Send
            + Sync
            + 'static
            + Clone
            + Eq
            + std::hash::Hash,
        V: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static + Clone,
    {
        self.open_store_with_default(table_name, || Box::new(StorageHashMap::new()))
    }

    /// Opens the kv store for `table_name`, using `default_storage_constructor`
    /// when the backend is [`StorageBackend::Memory`].
    pub fn open_store_with_default<K, V, F>(
        &self,
        table_name: &str,
        default_storage_constructor: F,
    ) -> StorageResult<Box<dyn Storage<K, V>>>
    where
        K: serde::Serialize
            + serde::de::DeserializeOwned
            + Send
            + Sync
            + 'static
            + Clone
            + Eq
            + std::hash::Hash,
        V: serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static + Clone,
        F: FnOnce() -> Box<dyn Storage<K, V>>,
    {
        #[cfg(not(any(feature = "sqlite", feature = "postgres")))]
        let _ = table_name;
        match self {
            StorageBackend::Memory => Ok(default_storage_constructor()),
            #[cfg(feature = "sqlite")]
            StorageBackend::Sqlite(backend, failed) => Ok(Box::new(Watched {
                inner: Box::new(backend.open_store(table_name)?),
                failed: failed.clone(),
            })),
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(backend, failed) => Ok(Box::new(Watched {
                inner: Box::new(backend.open_store(table_name)?),
                failed: failed.clone(),
            })),
        }
    }

    /// Whether stores opened on this backend survive process restart when
    /// pointed at the same database.
    pub fn is_persistent(&self) -> bool {
        !matches!(self, StorageBackend::Memory)
    }

    /// Whether any write to a store of this backend has failed.
    pub fn write_failed(&self) -> bool {
        match self {
            StorageBackend::Memory => false,
            #[cfg(feature = "sqlite")]
            StorageBackend::Sqlite(_, failed) => failed.load(Ordering::Acquire),
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(_, failed) => failed.load(Ordering::Acquire),
        }
    }

    /// Releases backend resources that need explicit cleanup before exit.
    /// For SQLite this checkpoints the WAL and removes the `-wal`/`-shm`
    /// files; the other backends have nothing to flush.
    pub fn shutdown(&self) {
        #[cfg(feature = "sqlite")]
        if let StorageBackend::Sqlite(backend, _) = self {
            backend.checkpoint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageError;

    /// A store whose writes fail once `failing` is set.
    #[derive(Clone, Default)]
    struct Flaky {
        inner: StorageHashMap<u64, u64>,
        failing: bool,
    }

    impl Storage<u64, u64> for Flaky {
        fn store(&mut self, key: u64, value: u64) -> StorageResult<()> {
            match self.failing {
                true => Err(StorageError::LockError),
                false => Storage::store(&mut self.inner, key, value),
            }
        }
        fn clear(&mut self) -> StorageResult<()> {
            Storage::<u64, u64>::clear(&mut self.inner)
        }
        fn get(&self, key: &u64) -> StorageResult<Option<u64>> {
            Storage::get(&self.inner, key)
        }
        fn take(&mut self, key: &u64) -> StorageResult<Option<u64>> {
            Storage::take(&mut self.inner, key)
        }
        fn keys(&self) -> StorageResult<Vec<u64>> {
            Storage::<u64, u64>::keys(&self.inner)
        }
        fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (u64, u64)> + '_>> {
            Storage::<u64, u64>::into_iter(&self.inner)
        }
        fn count(&self) -> StorageResult<u64> {
            Storage::<u64, u64>::count(&self.inner)
        }
        fn clone_box(&self) -> Box<dyn Storage<u64, u64>> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn a_failed_write_is_recorded_for_every_store_of_the_backend() {
        let failed = Arc::new(AtomicBool::new(false));
        let mut store = Watched {
            inner: Box::new(Flaky::default()),
            failed: failed.clone(),
        };
        store.store(1, 1).unwrap();
        assert_eq!(store.get(&1).unwrap(), Some(1), "reads pass through");
        assert!(
            !failed.load(Ordering::Acquire),
            "successful writes are not failures"
        );
        let mut flaky = Watched {
            inner: Box::new(Flaky {
                failing: true,
                ..Flaky::default()
            }),
            failed: failed.clone(),
        };
        assert!(
            flaky.store(2, 2).is_err(),
            "the failure itself still surfaces"
        );
        assert!(failed.load(Ordering::Acquire));
        // A copy (for example an overlay's base) reports into the same flag.
        let shared = Arc::new(AtomicBool::new(false));
        let original = Watched {
            inner: flaky.inner.clone_box(),
            failed: shared.clone(),
        };
        assert!(original.clone_box().store(3, 3).is_err());
        assert!(shared.load(Ordering::Acquire));
    }
}
