use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use log::debug;
use serde::{Deserialize, Serialize};
use surfpool_db::diesel::{
    self, RunQueryDsl,
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

/// Global shared connection pools keyed by database URL.
/// This allows multiple PostgresStorage instances to share the same pool,
/// which is essential for tests that run in parallel.
static SHARED_POOLS: OnceLock<
    Mutex<HashMap<String, Pool<ConnectionManager<diesel::PgConnection>>>>,
> = OnceLock::new();

fn get_or_create_shared_pool(
    database_url: &str,
) -> StorageResult<Pool<ConnectionManager<diesel::PgConnection>>> {
    let pools = SHARED_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pools_guard = pools.lock().map_err(|_| StorageError::LockError)?;

    if let Some(pool) = pools_guard.get(database_url) {
        debug!(
            "Reusing existing shared PostgreSQL connection pool for {}",
            database_url
        );
        return Ok(pool.clone());
    }

    debug!(
        "Creating new shared PostgreSQL connection pool for {}",
        database_url
    );
    let manager = ConnectionManager::<diesel::PgConnection>::new(database_url);
    let pool = Pool::builder()
        .max_size(10) // Limit total connections across all tests
        .min_idle(Some(1))
        .build(manager)
        .map_err(|e| StorageError::PooledConnectionError(NAME.into(), e))?;

    pools_guard.insert(database_url.to_string(), pool.clone());
    Ok(pool)
}

/// The PostgreSQL side of a [`super::StorageBackend`]. Holds a lease on the
/// process-level pool for its database URL: unlike SQLite, the pool stays
/// shared across surfnets, since pooling exists to amortize the network
/// connection and the server caps total sessions.
#[derive(Clone)]
pub struct PostgresBackend {
    pool: Pool<ConnectionManager<diesel::PgConnection>>,
    surfnet_id: String,
}

impl PostgresBackend {
    pub fn open(database_url: &str, surfnet_id: &str) -> StorageResult<Self> {
        debug!(
            "Opening PostgreSQL backend for database: {} with surfnet_id: {}",
            database_url, surfnet_id
        );
        let pool = get_or_create_shared_pool(database_url)?;
        Ok(PostgresBackend {
            pool,
            surfnet_id: surfnet_id.to_string(),
        })
    }

    pub fn open_store<K, V>(&self, table_name: &str) -> StorageResult<PostgresStorage<K, V>>
    where
        K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
        V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let storage = PostgresStorage {
            pool: self.pool.clone(),
            _phantom: std::marker::PhantomData,
            table_name: table_name.to_string(),
            surfnet_id: self.surfnet_id.clone(),
        };
        storage.ensure_table_exists()?;
        debug!(
            "PostgreSQL storage connected successfully for table: {}",
            table_name
        );
        Ok(storage)
    }
}

#[derive(Clone)]
pub struct PostgresStorage<K, V> {
    pool: Pool<ConnectionManager<diesel::PgConnection>>,
    _phantom: std::marker::PhantomData<(K, V)>,
    table_name: String,
    surfnet_id: String,
}

const NAME: &str = "PostgreSQL";

impl<K, V> PostgresStorage<K, V>
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
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
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
            "SELECT value FROM {} WHERE surfnet_id = $1 AND key = $2",
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

impl<K, V> Storage<K, V> for PostgresStorage<K, V>
where
    K: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
    V: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn store(&mut self, key: K, value: V) -> StorageResult<()> {
        debug!("Storing value in table '{}", self.table_name);
        let key_str = serialize_key(NAME, &self.table_name, &key)?;
        let value_str = serialize_value(NAME, &self.table_name, &value)?;

        // Use PostgreSQL UPSERT syntax with ON CONFLICT
        let query = sql_query(format!(
            "INSERT INTO {} (surfnet_id, key, value, updated_at) VALUES ($1, $2, $3, CURRENT_TIMESTAMP)
             ON CONFLICT (surfnet_id, key) DO UPDATE SET
             value = EXCLUDED.value,
             updated_at = CURRENT_TIMESTAMP",
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
                "DELETE FROM {} WHERE surfnet_id = $1 AND key = $2",
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
            "DELETE FROM {} WHERE surfnet_id = $1",
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
            "SELECT key FROM {} WHERE surfnet_id = $1",
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
            "SELECT COUNT(*) as count FROM {} WHERE surfnet_id = $1",
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
            "SELECT key, value FROM {} WHERE surfnet_id = $1",
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
