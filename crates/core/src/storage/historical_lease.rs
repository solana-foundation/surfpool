//! Exclusive ownership of a persistent historical namespace, independent of cache/journal paths.
use std::fs::{File, OpenOptions, TryLockError};

use sha2::{Digest, Sha256};

/// Keep this guard alive until all writers have stopped. OS/server locks release on exit;
/// lock files are deliberately never unlinked, so a new opener cannot bypass an old inode.
pub enum HistoricalStoreLease {
    Sqlite(File),
    #[cfg(feature = "postgres")]
    Postgres(surfpool_db::diesel::PgConnection),
}

impl HistoricalStoreLease {
    pub fn acquire(database: Option<&str>, namespace: &str) -> Result<Option<Self>, String> {
        Self::open(database, namespace, true)
    }

    /// Ordinary sessions may coexist as before, but cannot race a historical writer.
    pub fn normal(database: Option<&str>, namespace: &str) -> Result<Option<Self>, String> {
        Self::open(database, namespace, false)
    }

    fn open(
        database: Option<&str>,
        namespace: &str,
        exclusive: bool,
    ) -> Result<Option<Self>, String> {
        let Some(database) = database.filter(|db| *db != ":memory:") else {
            return Ok(None);
        };
        if database.starts_with("postgres://") || database.starts_with("postgresql://") {
            #[cfg(feature = "postgres")]
            {
                return Self::postgres(database, namespace, exclusive).map(Some);
            }
            #[cfg(not(feature = "postgres"))]
            return Err("SESSION_LEASE_IO: PostgreSQL support is not enabled".into());
        }
        #[cfg(feature = "sqlite")]
        {
            Self::sqlite(database, namespace, exclusive)
        }
        #[cfg(not(feature = "sqlite"))]
        Err("SESSION_LEASE_IO: SQLite support is not enabled".into())
    }

    #[cfg(feature = "sqlite")]
    fn sqlite(database: &str, namespace: &str, exclusive: bool) -> Result<Option<Self>, String> {
        use surfpool_db::diesel::{
            self, Connection, RunQueryDsl, SqliteConnection, sql_types::Text,
        };
        #[derive(diesel::QueryableByName)]
        struct Location {
            #[diesel(sql_type = Text)]
            file: String,
        }
        // Ask SQLite which file it actually opened: relative paths, symlinks and URI escaping
        // must name the same lease. No Surfpool tables or state are written here.
        let mut connection = SqliteConnection::establish(database)
            .map_err(|_| "SESSION_LEASE_IO: cannot open SQLite database")?;
        let locations = diesel::sql_query("PRAGMA database_list")
            .load::<Location>(&mut connection)
            .map_err(|_| "SESSION_LEASE_IO: cannot resolve SQLite database")?;
        let location = locations
            .first()
            .ok_or("SESSION_LEASE_IO: SQLite main database is missing")?;
        if location.file.is_empty() {
            return if exclusive {
                Err("SESSION_LEASE_IO: historical persistence requires a file-backed SQLite database".into())
            } else {
                Ok(None)
            };
        }
        let path = std::fs::canonicalize(&location.file)
            .map_err(|error| format!("SESSION_LEASE_IO: {error}"))?;
        let mut name = path
            .file_name()
            .ok_or("SESSION_LEASE_IO: database has no file name")?
            .to_os_string();
        name.push(format!(
            ".surfpool-{}.lock",
            hex::encode(Sha256::digest(namespace.as_bytes()))
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_file_name(name))
            .map_err(|error| format!("SESSION_LEASE_IO: {error}"))?;
        let locked = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        locked.map_err(|error| match error {
            TryLockError::WouldBlock => {
                "SESSION_IN_USE: another historical Surfpool owns this database namespace".into()
            }
            TryLockError::Error(error) => format!("SESSION_LEASE_IO: {error}"),
        })?;
        Ok(Some(Self::Sqlite(file)))
    }

    #[cfg(feature = "postgres")]
    fn postgres(database: &str, namespace: &str, exclusive: bool) -> Result<Self, String> {
        use surfpool_db::diesel::{
            self, Connection, RunQueryDsl,
            sql_types::{Bool, Text},
        };
        #[derive(diesel::QueryableByName)]
        struct Acquired {
            #[diesel(sql_type = Bool)]
            acquired: bool,
        }
        let mut connection = diesel::PgConnection::establish(database)
            .map_err(|_| "SESSION_LEASE_IO: cannot connect to PostgreSQL")?;
        let query = if exclusive {
            "SELECT pg_try_advisory_lock(hashtextextended($1, 0)) AS acquired"
        } else {
            "SELECT pg_try_advisory_lock_shared(hashtextextended($1, 0)) AS acquired"
        };
        let lock = diesel::sql_query(query)
            .bind::<Text, _>(format!("surfpool-historical-writer/v1|{namespace}"))
            .get_result::<Acquired>(&mut connection)
            .map_err(|_| "SESSION_LEASE_IO: cannot acquire PostgreSQL namespace lease")?;
        if !lock.acquired {
            return Err(
                "SESSION_IN_USE: another historical Surfpool owns this database namespace".into(),
            );
        }
        Ok(Self::Postgres(connection))
    }
}

#[cfg(test)]
mod tests;
