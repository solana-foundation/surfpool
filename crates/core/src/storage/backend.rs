use crate::storage::{Storage, StorageHashMap, StorageResult};

/// Per-surfnet storage backend. Owns the connection resources for one surfnet
/// (the SQLite pool, or a lease on the shared PostgreSQL pool) and mints the
/// per-table stores that share them. Constructed once per surfnet from the
/// database URL; dropping the surfnet drops the backend and its connections.
#[derive(Clone)]
pub enum StorageBackend {
    /// No database URL: stores are independent in-process maps.
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(super::sqlite::SqliteBackend),
    #[cfg(feature = "postgres")]
    Postgres(super::postgres::PostgresBackend),
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
                return Ok(StorageBackend::Postgres(backend));
            }
            #[cfg(not(feature = "postgres"))]
            return Err(super::StorageError::PostgresNotEnabled);
        }
        #[cfg(feature = "sqlite")]
        {
            let backend = super::sqlite::SqliteBackend::open(url, surfnet_id)?;
            Ok(StorageBackend::Sqlite(backend))
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
            StorageBackend::Sqlite(backend) => Ok(Box::new(backend.open_store(table_name)?)),
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(backend) => Ok(Box::new(backend.open_store(table_name)?)),
        }
    }

    /// Whether stores opened on this backend survive process restart when
    /// pointed at the same database.
    pub fn is_persistent(&self) -> bool {
        !matches!(self, StorageBackend::Memory)
    }

    /// Releases backend resources that need explicit cleanup before exit.
    /// For SQLite this checkpoints the WAL and removes the `-wal`/`-shm`
    /// files; the other backends have nothing to flush.
    pub fn shutdown(&self) {
        #[cfg(feature = "sqlite")]
        if let StorageBackend::Sqlite(backend) = self {
            backend.checkpoint();
        }
    }
}
