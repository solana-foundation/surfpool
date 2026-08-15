use std::sync::atomic::{AtomicU64, Ordering};

use log::debug;
use serde::{Deserialize, Serialize};
use surfpool_db::diesel::{
    self, RunQueryDsl, SqliteConnection,
    connection::SimpleConnection,
    r2d2::{ConnectionManager, Pool},
    sql_query,
    sql_types::Text,
};

use crate::storage::{
    Storage, StorageError, StorageResult,
    diesel_common::{
        CountRecord, KeyRecord, KvRecord, ValueRecord, deserialize_value, serialize_key,
        serialize_value,
    },
};

type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;

/// Applies pragmas when each pool connection is created.
#[derive(Debug)]
struct SqlitePragmaCustomizer {
    is_file_based: bool,
}

impl diesel::r2d2::CustomizeConnection<SqliteConnection, diesel::r2d2::Error>
    for SqlitePragmaCustomizer
{
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        let pragmas = if self.is_file_based {
            "PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=268435456; PRAGMA cache_size=-64000; PRAGMA busy_timeout=5000;"
        } else {
            "PRAGMA synchronous=OFF; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-64000; PRAGMA busy_timeout=5000;"
        };
        conn.batch_execute(pragmas)
            .map_err(diesel::r2d2::Error::QueryError)
    }
}

/// Counter for unique in-memory database names, so that separate backends
/// (separate surfnets) get separate in-memory databases.
static MEMORY_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The SQLite side of a [`super::StorageBackend`]: one connection pool per
/// surfnet, shared by every store opened on it. The pool lives exactly as
/// long as the backend (and the stores holding clones of it), so dropping
/// the surfnet releases the connections and their descriptors.
#[derive(Clone)]
pub struct SqliteBackend {
    pool: SqlitePool,
    connection_string: String,
    is_file_based: bool,
    surfnet_id: String,
}

impl SqliteBackend {
    pub fn open(database_url: &str, surfnet_id: &str) -> StorageResult<Self> {
        debug!(
            "Opening SQLite backend for database: {} with surfnet_id: {}",
            database_url, surfnet_id
        );

        let connection_string = if database_url == ":memory:" {
            // Unique name per backend; cache=shared so the pool's connections
            // (and therefore all of this surfnet's stores) share one database.
            let id = MEMORY_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
            format!("file:memdb{}?mode=memory&cache=shared", id)
        } else if database_url.starts_with("file:") {
            if database_url.contains('?') {
                format!("{}&mode=rwc", database_url)
            } else {
                format!("{}?mode=rwc", database_url)
            }
        } else {
            format!("file:{}?mode=rwc", database_url)
        };

        let is_file_based = database_url != ":memory:";
        let manager = ConnectionManager::new(&connection_string);
        let pool = Pool::builder()
            .max_size(10)
            .connection_customizer(Box::new(SqlitePragmaCustomizer { is_file_based }))
            .build(manager)
            .map_err(|e| StorageError::PooledConnectionError(NAME.into(), e))?;

        // journal_mode=WAL persists to file; wal_autocheckpoint is per-connection
        if is_file_based {
            let mut conn = pool.get().map_err(|_| StorageError::LockError)?;
            conn.batch_execute("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=1000;")
                .map_err(|e| StorageError::create_table("pragma_init", NAME, e))?;
        }

        Ok(SqliteBackend {
            pool,
            connection_string,
            is_file_based,
            surfnet_id: surfnet_id.to_string(),
        })
    }

    pub fn open_store<K, V>(&self, table_name: &str) -> StorageResult<SqliteStorage<K, V>>
    where
        K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
        V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let storage = SqliteStorage {
            pool: self.pool.clone(),
            _phantom: std::marker::PhantomData,
            table_name: table_name.to_string(),
            surfnet_id: self.surfnet_id.clone(),
        };
        storage.ensure_table_exists()?;
        debug!(
            "SQLite storage connected successfully for table: {}",
            table_name
        );
        Ok(storage)
    }

    /// Checkpoint the WAL and truncate it to consolidate into the main
    /// database file, then remove the -wal and -shm files. Only runs for
    /// file-based databases (not :memory:). Safe to call more than once.
    pub(super) fn checkpoint(&self) {
        if !self.is_file_based {
            return;
        }

        // Extract the file path from the connection string
        // Connection string is like "file:/path/to/db.sqlite?mode=rwc"
        let db_path = self
            .connection_string
            .strip_prefix("file:")
            .and_then(|s| s.split('?').next())
            .unwrap_or(&self.connection_string)
            .to_string();

        debug!("Checkpointing WAL for database '{}'", db_path);

        // Use pool connection to checkpoint - this flushes WAL to main database
        if let Ok(mut conn) = self.pool.get() {
            if let Err(e) = conn.batch_execute("PRAGMA wal_checkpoint(TRUNCATE);") {
                debug!("WAL checkpoint failed: {}", e);
                return;
            }
        }

        // Remove the -wal and -shm files
        let wal_path = format!("{}-wal", db_path);
        let shm_path = format!("{}-shm", db_path);

        if std::path::Path::new(&wal_path).exists() {
            if let Err(e) = std::fs::remove_file(&wal_path) {
                debug!("Failed to remove WAL file {}: {}", wal_path, e);
            } else {
                debug!("Removed WAL file: {}", wal_path);
            }
        }

        if std::path::Path::new(&shm_path).exists() {
            if let Err(e) = std::fs::remove_file(&shm_path) {
                debug!("Failed to remove SHM file {}: {}", shm_path, e);
            } else {
                debug!("Removed SHM file: {}", shm_path);
            }
        }
    }
}

#[derive(Clone)]
pub struct SqliteStorage<K, V> {
    pool: SqlitePool,
    _phantom: std::marker::PhantomData<(K, V)>,
    table_name: String,
    surfnet_id: String,
}

const NAME: &str = "SQLite";

impl<K, V> SqliteStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de>,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
{
    fn ensure_table_exists(&self) -> StorageResult<()> {
        debug!("Ensuring table '{}' exists", self.table_name);
        let create_table_sql = format!(
            "
            CREATE TABLE IF NOT EXISTS {} (
                surfnet_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (surfnet_id, key)
            )
        ",
            self.table_name
        );

        debug!("Getting connection from pool for table creation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        conn.batch_execute(&create_table_sql)
            .map_err(|e| StorageError::create_table(&self.table_name, NAME, e))?;

        debug!("Successfully ensured table '{}' exists", self.table_name);
        Ok(())
    }

    fn load_value_from_db(&self, key_str: &str) -> StorageResult<Option<V>> {
        debug!("Loading value from DB for key: {}", key_str);
        let query = sql_query(format!(
            "SELECT value FROM {} WHERE surfnet_id = ? AND key = ?",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id)
        .bind::<Text, _>(key_str);

        trace!("Getting connection from pool for loading value");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        let records = query
            .load::<ValueRecord>(&mut *conn)
            .map_err(|e| StorageError::get(&self.table_name, NAME, key_str, e))?;

        if let Some(record) = records.into_iter().next() {
            debug!("Found record for key: {}", key_str);
            let value = deserialize_value(NAME, &self.table_name, &record.value)?;
            Ok(Some(value))
        } else {
            debug!("No record found for key: {}", key_str);
            Ok(None)
        }
    }
}

impl<K, V> Storage<K, V> for SqliteStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn store(&mut self, key: K, value: V) -> StorageResult<()> {
        debug!("Storing value in table '{}", self.table_name);
        let key_str = serialize_key(NAME, &self.table_name, &key)?;
        let value_str = serialize_value(NAME, &self.table_name, &value)?;

        // Use prepared statement with sql_query for better safety
        let query = sql_query(format!(
            "INSERT OR REPLACE INTO {} (surfnet_id, key, value, updated_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id)
        .bind::<Text, _>(&key_str)
        .bind::<Text, _>(&value_str);

        trace!("Getting connection from pool for store operation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        query
            .execute(&mut *conn)
            .map_err(|e| StorageError::store(&self.table_name, NAME, &key_str, e))?;

        debug!("Value stored successfully in table '{}'", self.table_name);
        Ok(())
    }

    fn get(&self, key: &K) -> StorageResult<Option<V>> {
        debug!("Getting value from table '{}", self.table_name);
        let key_str = serialize_key(NAME, &self.table_name, key)?;

        self.load_value_from_db(&key_str)
    }

    fn take(&mut self, key: &K) -> StorageResult<Option<V>> {
        debug!("Taking value from table '{}'", self.table_name);
        let key_str = serialize_key(NAME, &self.table_name, key)?;

        // If not in cache, try to load from database
        if let Some(value) = self.load_value_from_db(&key_str)? {
            debug!("Value found, removing from database");
            // Remove from database
            let delete_query = sql_query(format!(
                "DELETE FROM {} WHERE surfnet_id = ? AND key = ?",
                self.table_name
            ))
            .bind::<Text, _>(&self.surfnet_id)
            .bind::<Text, _>(&key_str);

            trace!("Getting connection from pool for delete operation");
            let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

            delete_query
                .execute(&mut *conn)
                .map_err(|e| StorageError::delete(&self.table_name, NAME, &key_str, e))?;

            debug!(
                "Value taken and removed successfully from table '{}'",
                self.table_name
            );
            Ok(Some(value))
        } else {
            debug!("No value found to take from table '{}'", self.table_name);
            Ok(None)
        }
    }

    fn clear(&mut self) -> StorageResult<()> {
        debug!("Clearing all data from table '{}'", self.table_name);
        let delete_query = sql_query(format!(
            "DELETE FROM {} WHERE surfnet_id = ?",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id);

        trace!("Getting connection from pool for clear operation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        delete_query
            .execute(&mut *conn)
            .map_err(|e| StorageError::delete(&self.table_name, NAME, "*all*", e))?;

        debug!("Table '{}' cleared successfully", self.table_name);
        Ok(())
    }

    fn keys(&self) -> StorageResult<Vec<K>> {
        debug!("Fetching all keys from table '{}'", self.table_name);
        let query = sql_query(format!(
            "SELECT key FROM {} WHERE surfnet_id = ?",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id);

        trace!("Getting connection from pool for keys operation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        let records = query
            .load::<KeyRecord>(&mut *conn)
            .map_err(|e| StorageError::get_all_keys(&self.table_name, NAME, e))?;

        let mut keys = Vec::new();
        for record in records {
            let key: K = serde_json::from_str(&record.key)
                .map_err(|e| StorageError::DeserializeValueError(NAME.into(), e))?;
            keys.push(key);
        }

        debug!(
            "Retrieved {} keys from table '{}'",
            keys.len(),
            self.table_name
        );
        Ok(keys)
    }

    fn clone_box(&self) -> Box<dyn Storage<K, V>> {
        Box::new(self.clone())
    }

    fn count(&self) -> StorageResult<u64> {
        debug!("Counting entries in table '{}'", self.table_name);
        let query = sql_query(format!(
            "SELECT COUNT(*) as count FROM {} WHERE surfnet_id = ?",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id);

        trace!("Getting connection from pool for count operation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        let records = query
            .load::<CountRecord>(&mut *conn)
            .map_err(|e| StorageError::count(&self.table_name, NAME, e))?;

        let count = records.first().map(|r| r.count as u64).unwrap_or(0);
        debug!("Table '{}' has {} entries", self.table_name, count);
        Ok(count)
    }

    fn into_iter(&self) -> StorageResult<Box<dyn Iterator<Item = (K, V)> + '_>> {
        debug!(
            "Creating iterator for all key-value pairs in table '{}'",
            self.table_name
        );
        let query = sql_query(format!(
            "SELECT key, value FROM {} WHERE surfnet_id = ?",
            self.table_name
        ))
        .bind::<Text, _>(&self.surfnet_id);

        trace!("Getting connection from pool for into_iter operation");
        let mut conn = self.pool.get().map_err(|_| StorageError::LockError)?;

        let records = query
            .load::<KvRecord>(&mut *conn)
            .map_err(|e| StorageError::get_all_key_value_pairs(&self.table_name, NAME, e))?;

        let iter = records.into_iter().filter_map(move |record| {
            let key: K = match serde_json::from_str(&record.key) {
                Ok(k) => k,
                Err(e) => {
                    debug!("Failed to deserialize key: {}", e);
                    return None;
                }
            };
            let value: V = match serde_json::from_str(&record.value) {
                Ok(v) => v,
                Err(e) => {
                    debug!("Failed to deserialize value: {}", e);
                    return None;
                }
            };
            Some((key, value))
        });

        debug!(
            "Iterator created successfully for table '{}'",
            self.table_name
        );
        Ok(Box::new(iter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(diesel::QueryableByName, Debug)]
    struct PragmaInt {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        value: i64,
    }

    /// Stores opened on one backend share its connection pool.
    #[test]
    fn test_stores_share_the_backend_pool() {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        let backend = SqliteBackend::open(db_path, "surfnet1").unwrap();
        let storage1: SqliteStorage<String, String> = backend.open_store("table1").unwrap();
        let storage2: SqliteStorage<String, String> = backend.open_store("table2").unwrap();
        let storage3: SqliteStorage<String, String> = backend.open_store("table3").unwrap();

        let _conn1 = storage1.pool.get().unwrap();

        let state1 = storage1.pool.state();
        let state2 = storage2.pool.state();
        let state3 = storage3.pool.state();

        assert_eq!(
            state1.connections, state2.connections,
            "pools should be shared"
        );
        assert_eq!(
            state2.connections, state3.connections,
            "pools should be shared"
        );
        assert_eq!(
            state1.idle_connections, state2.idle_connections,
            "pools should be shared"
        );
    }

    /// Pragmas are applied to all pooled connections, not just the first.
    #[test]
    fn test_pragmas_applied_to_pooled_connections() {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        let backend = SqliteBackend::open(db_path, "surfnet1").unwrap();
        let storage: SqliteStorage<String, String> = backend.open_store("pragma_test").unwrap();

        let _conn1 = storage.pool.get().unwrap();
        let mut conn2 = storage.pool.get().unwrap();

        let cache_result: Vec<PragmaInt> =
            diesel::sql_query("SELECT cache_size as value FROM pragma_cache_size")
                .load(&mut *conn2)
                .unwrap();
        assert_eq!(cache_result[0].value, -64000, "cache_size should be -64000");

        let timeout_result: Vec<PragmaInt> =
            diesel::sql_query("SELECT timeout as value FROM pragma_busy_timeout")
                .load(&mut *conn2)
                .unwrap();
        assert_eq!(timeout_result[0].value, 5000, "busy_timeout should be 5000");
    }

    /// File-specific pragmas (synchronous, temp_store) are applied to all connections.
    #[test]
    fn test_pragmas_applied_to_file_database() {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        let db_path = temp_file.path().to_str().unwrap();

        let backend = SqliteBackend::open(db_path, "surfnet1").unwrap();
        let storage: SqliteStorage<String, String> = backend.open_store("pragma_test").unwrap();

        let _conn1 = storage.pool.get().unwrap();
        let mut conn2 = storage.pool.get().unwrap();

        let result: Vec<PragmaInt> =
            diesel::sql_query("SELECT synchronous as value FROM pragma_synchronous")
                .load(&mut *conn2)
                .unwrap();
        assert_eq!(result[0].value, 1, "synchronous should be NORMAL (1)");

        let temp_result: Vec<PragmaInt> =
            diesel::sql_query("SELECT temp_store as value FROM pragma_temp_store")
                .load(&mut *conn2)
                .unwrap();
        assert_eq!(temp_result[0].value, 2, "temp_store should be MEMORY (2)");
    }

    /// In-memory databases are isolated between backends (surfnets).
    #[test]
    fn test_in_memory_backends_are_isolated() {
        let backend1 = SqliteBackend::open(":memory:", "surfnet1").unwrap();
        let backend2 = SqliteBackend::open(":memory:", "surfnet1").unwrap();

        let mut storage1: SqliteStorage<String, String> =
            backend1.open_store("test_table").unwrap();
        let storage2: SqliteStorage<String, String> = backend2.open_store("test_table").unwrap();

        storage1
            .store("key1".to_string(), "value1".to_string())
            .unwrap();

        let result = storage2.get(&"key1".to_string()).unwrap();
        assert!(result.is_none(), "in-memory backends should be isolated");
    }

    /// Within one backend, in-memory stores share the database, matching the
    /// on-disk behavior (distinct tables in one database file).
    #[test]
    fn test_in_memory_stores_share_one_database() {
        let backend = SqliteBackend::open(":memory:", "surfnet1").unwrap();

        let mut storage1: SqliteStorage<String, String> = backend.open_store("table_a").unwrap();
        let storage2: SqliteStorage<String, String> = backend.open_store("table_a").unwrap();

        storage1
            .store("key1".to_string(), "value1".to_string())
            .unwrap();

        let result = storage2.get(&"key1".to_string()).unwrap();
        assert_eq!(
            result,
            Some("value1".to_string()),
            "stores on one backend should see each other's writes"
        );
    }
}
