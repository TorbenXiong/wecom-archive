use crate::{ArchiveReadSnapshot, ArchiveStore, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

impl ArchiveStore {
    pub fn read_snapshot(&self) -> Result<ArchiveReadSnapshot<'_>, StoreError> {
        Ok(ArchiveReadSnapshot {
            _transaction: self.connection.unchecked_transaction()?,
        })
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with_busy_timeout(path, Duration::from_secs(30))
    }

    pub fn open_with_busy_timeout(path: &Path, busy_timeout: Duration) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Invalid(error.to_string()))?;
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Retain the bounded set of insertion and revision statements, including
        // compiled trigger programs, across mixed batches.
        connection.set_prepared_statement_cache_capacity(32);
        connection.busy_timeout(busy_timeout)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        Self::open_read_only_with_busy_timeout(path, Duration::from_secs(30))
    }

    pub fn open_read_only_with_busy_timeout(
        path: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(busy_timeout)?;
        connection.pragma_update(None, "query_only", true)?;
        Ok(Self { connection })
    }
}
