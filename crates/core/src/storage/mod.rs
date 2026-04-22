#[cfg(any(feature = "postgres", feature = "sqlite"))]
mod diesel_common;
mod fifo_map;
mod hash_map;
mod overlay;
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "sqlite")]
mod sqlite;
pub use hash_map::HashMap as StorageHashMap;
pub use overlay::OverlayStorage;
#[cfg(feature = "postgres")]
pub use postgres::PostgresStorage;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStorage;
pub use surfpool_types::FifoMap as StorageFifoMap;

use crate::error::SurfpoolError;

pub fn new_kv_store<K, V>(
    database_url: &Option<&str>,
    table_name: &str,
    surfnet_id: &str,
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
{
    new_kv_store_with_default(database_url, table_name, surfnet_id, || {
        Box::new(StorageHashMap::new())
    })
}

pub fn new_kv_store_with_default<K, V, F>(
    database_url: &Option<&str>,
    table_name: &str,
    surfnet_id: &str,
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
    match database_url {
        Some(url) => {
            #[cfg(feature = "postgres")]
            if url.starts_with("postgres://") || url.starts_with("postgresql://") {
                let storage = PostgresStorage::connect(url, table_name, surfnet_id)?;
                Ok(Box::new(storage))
            } else {
                #[cfg(feature = "sqlite")]
                {
                    let storage = SqliteStorage::connect(url, table_name, surfnet_id)?;
                    Ok(Box::new(storage))
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    Err(StorageError::InvalidPostgresUrl(url.to_string()))
                }
            }

            #[cfg(not(feature = "postgres"))]
            if url.starts_with("postgres://") || url.starts_with("postgresql://") {
                Err(StorageError::PostgresNotEnabled)
            } else {
                #[cfg(feature = "sqlite")]
                {
                    let storage = SqliteStorage::connect(
                        database_url.unwrap_or(":memory:"),
                        table_name,
                        surfnet_id,
                    )?;
                    Ok(Box::new(storage))
                }
                #[cfg(not(feature = "sqlite"))]
                {
                    Err(StorageError::SqliteNotEnabled)
                }
            }
        }
        _ => {
            let storage = default_storage_constructor();
            Ok(storage)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Sqlite storage is not enabled in this build")]
    SqliteNotEnabled,
    #[error("Postgres storage is not enabled in this build")]
    PostgresNotEnabled,
    #[error("Invalid Postgres database URL: {0}")]
    InvalidPostgresUrl(String),
    #[error("Failed to get pooled connection for '{0}' database: {1}")]
    PooledConnectionError(String, #[source] surfpool_db::diesel::r2d2::PoolError),
    #[error("Failed to serialize key for '{0}' database: {1}")]
    SerializeKeyError(String, serde_json::Error),
    #[error("Failed to serialize value for '{0}' database: {1}")]
    SerializeValueError(String, serde_json::Error),
    #[error("Failed to deserialize value in '{0}' database: {1}")]
    DeserializeValueError(String, serde_json::Error),
    #[error("Failed to acquire lock for database")]
    LockError,
    #[error("Query failed for table '{0}' in '{1}' database: {2}")]
    QueryError(String, String, #[source] QueryExecuteError),
}

impl StorageError {
    pub fn create_table(
        table_name: &str,
        db_type: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::CreateTableError(e),
        )
    }
    pub fn store(
        table_name: &str,
        db_type: &str,
        store_key: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::StoreError(store_key.to_string(), e),
        )
    }
    pub fn get(
        table_name: &str,
        db_type: &str,
        get_key: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::GetError(get_key.to_string(), e),
        )
    }
    pub fn delete(
        table_name: &str,
        db_type: &str,
        delete_key: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::DeleteError(delete_key.to_string(), e),
        )
    }
    pub fn get_all_keys(
        table_name: &str,
        db_type: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::GetAllKeysError(e),
        )
    }
    pub fn get_all_key_value_pairs(
        table_name: &str,
        db_type: &str,
        e: surfpool_db::diesel::result::Error,
    ) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::GetAllKeyValuePairsError(e),
        )
    }
    pub fn count(table_name: &str, db_type: &str, e: surfpool_db::diesel::result::Error) -> Self {
        StorageError::QueryError(
            table_name.to_string(),
            db_type.to_string(),
            QueryExecuteError::CountError(e),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QueryExecuteError {
    #[error("Failed to create table: {0}")]
    CreateTableError(#[source] surfpool_db::diesel::result::Error),
    #[error("Failed to store value for key '{0}': {1}")]
    StoreError(String, #[source] surfpool_db::diesel::result::Error),
    #[error("Failed to get value for key '{0}': {1}")]
    GetError(String, #[source] surfpool_db::diesel::result::Error),
    #[error("Failed to delete value for key '{0}': {1}")]
    DeleteError(String, #[source] surfpool_db::diesel::result::Error),
    #[error("Failed to get all keys: {0}")]
    GetAllKeysError(#[source] surfpool_db::diesel::result::Error),
    #[error("Failed to get all key-value pairs: {0}")]
    GetAllKeyValuePairsError(#[source] surfpool_db::diesel::result::Error),
    #[error("Failed to count entries: {0}")]
    CountError(#[source] surfpool_db::diesel::result::Error),
}

pub type StorageResult<T> = Result<T, StorageError>;

impl From<StorageError> for jsonrpc_core::Error {
    fn from(err: StorageError) -> Self {
        SurfpoolError::from(err).into()
    }
}

pub trait Storage<K, V>: Send + Sync {
    fn store(&mut self, key: K, value: V) -> StorageResult<()>;
    fn clear(&mut self) -> StorageResult<()>;
    fn get(&self, key: &K) -> StorageResult<Option<V>>;
    fn take(&mut self, key: &K) -> StorageResult<Option<V>>;
    fn keys(&self) -> StorageResult<Vec<K>>;
    fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>>;
    fn contains_key(&self, key: &K) -> StorageResult<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Returns the number of entries in the storage.
    fn count(&self) -> StorageResult<u64>;

    /// Explicitly shutdown the storage, performing any cleanup like WAL checkpoint.
    /// This should be called before the application exits to ensure data is persisted.
    /// Default implementation does nothing.
    fn shutdown(&self) {}

    // Enable cloning of boxed trait objects
    fn clone_box(&self) -> Box<dyn Storage<K, V>>;
}

// Implement Clone for Box<dyn Storage<K, V>>
impl<K, V> Clone for Box<dyn Storage<K, V>> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

// Separate trait for construction - this doesn't need to be dyn-compatible
pub trait StorageConstructor<K, V>: Storage<K, V> + Clone {
    fn connect(database_url: &str, table_name: &str, surfnet_id: &str) -> StorageResult<Self>
    where
        Self: Sized;
}

#[cfg(test)]
pub mod tests {
    use std::os::unix::fs::PermissionsExt;

    use crossbeam_channel::Receiver;
    use surfpool_types::SimnetEvent;
    use uuid::Uuid;

    use crate::surfnet::{
        GeyserEvent,
        svm::{SurfnetSvm, SurfnetSvmConfig},
    };

    /// Environment variable for PostgreSQL database URL used in tests
    pub const POSTGRES_TEST_URL_ENV: &str = "SURFPOOL_TEST_POSTGRES_URL";

    /// Generates a random surfnet_id
    pub fn random_surfnet_id() -> String {
        let uuid = Uuid::new_v4();
        uuid.to_string()
    }

    pub enum TestType {
        NoDb,
        InMemorySqlite,
        OnDiskSqlite(String),
        /// PostgreSQL with a random surfnet_id for test isolation
        #[cfg(feature = "postgres")]
        Postgres {
            url: String,
            surfnet_id: String,
        },
    }

    impl TestType {
        pub fn initialize_svm(&self) -> (SurfnetSvm, Receiver<SimnetEvent>, Receiver<GeyserEvent>) {
            match &self {
                TestType::NoDb => SurfnetSvm::default(),
                TestType::InMemorySqlite => SurfnetSvm::new_with_db(
                    Some(":memory:"),
                    SurfnetSvmConfig {
                        surfnet_id: "0".to_string(),
                        ..SurfnetSvmConfig::default()
                    },
                )
                .unwrap(),
                TestType::OnDiskSqlite(db_path) => SurfnetSvm::new_with_db(
                    Some(db_path.as_ref()),
                    SurfnetSvmConfig {
                        surfnet_id: "0".to_string(),
                        ..SurfnetSvmConfig::default()
                    },
                )
                .unwrap(),
                #[cfg(feature = "postgres")]
                TestType::Postgres { url, surfnet_id } => SurfnetSvm::new_with_db(
                    Some(url.as_ref()),
                    SurfnetSvmConfig {
                        surfnet_id: surfnet_id.clone(),
                        ..SurfnetSvmConfig::default()
                    },
                )
                .unwrap(),
            }
        }

        pub fn sqlite() -> Self {
            let database_url = crate::storage::tests::create_tmp_sqlite_storage();
            TestType::OnDiskSqlite(database_url)
        }

        pub fn no_db() -> Self {
            TestType::NoDb
        }

        pub fn in_memory() -> Self {
            TestType::InMemorySqlite
        }

        /// Creates a PostgreSQL test type with a random surfnet_id for test isolation.
        /// The database URL is read from the SURFPOOL_TEST_POSTGRES_URL environment variable.
        /// Panics if the environment variable is not set.
        #[cfg(feature = "postgres")]
        pub fn postgres() -> Self {
            let url = std::env::var(POSTGRES_TEST_URL_ENV).unwrap_or_else(|_| {
                panic!(
                    "PostgreSQL test URL not set. Set the {} environment variable.",
                    POSTGRES_TEST_URL_ENV
                )
            });
            let surfnet_id = random_surfnet_id();
            println!(
                "Created PostgreSQL test connection with surfnet_id: {}",
                surfnet_id
            );
            TestType::Postgres { url, surfnet_id }
        }

        /// Creates a PostgreSQL test type with a random surfnet_id for test isolation.
        /// Returns None if the SURFPOOL_TEST_POSTGRES_URL environment variable is not set.
        #[cfg(feature = "postgres")]
        pub fn postgres_if_available() -> Option<Self> {
            std::env::var(POSTGRES_TEST_URL_ENV).ok().map(|url| {
                let surfnet_id = random_surfnet_id();
                println!(
                    "Created PostgreSQL test connection with surfnet_id: {}",
                    surfnet_id
                );
                TestType::Postgres { url, surfnet_id }
            })
        }
    }

    impl Drop for TestType {
        fn drop(&mut self) {
            if let TestType::OnDiskSqlite(db_path) = self {
                // Delete file at db_path when TestType goes out of scope
                let _ = std::fs::remove_file(db_path);
            }
            // Note: PostgreSQL data is isolated by surfnet_id and doesn't need cleanup
            // The random surfnet_id ensures test isolation without table cleanup
        }
    }

    pub fn create_tmp_sqlite_storage() -> String {
        // let temp_dir = tempfile::tempdir().expect("Failed to create temp dir for SqliteStorage");
        let write_permissions = std::fs::Permissions::from_mode(0o600);
        let file = tempfile::Builder::new()
            .permissions(write_permissions)
            .suffix(".sqlite")
            .tempfile()
            .expect("Failed to create temp file for SqliteStorage");
        let database_url = file.path().to_path_buf();

        // Use a simple path without creating the file beforehand
        // Let SQLite create the database file itself
        let database_url = database_url.to_str().unwrap().to_string();
        println!("Created temporary Sqlite database at: {}", database_url);
        database_url
    }
}
