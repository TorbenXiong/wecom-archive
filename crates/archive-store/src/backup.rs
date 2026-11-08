use crate::{ArchiveStore, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

impl ArchiveStore {
    /// Produce a standalone snapshot including committed WAL pages. Never copy
    /// a live database and its sidecars one file at a time.
    pub fn backup_to(source: &Path, destination: &Path) -> Result<(), StoreError> {
        let owned = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|_| StoreError::Invalid("BACKUP_TARGET_UNAVAILABLE".into()))?;
        drop(owned);
        let result = (|| {
            let connection = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.busy_timeout(std::time::Duration::from_secs(30))?;
            let target = destination
                .to_str()
                .ok_or_else(|| StoreError::Invalid("BACKUP_PATH_INVALID".into()))?;
            connection.execute("VACUUM INTO ?1", [target])?;
            Self::open_read_only(destination)?.verify_integrity()?;
            std::fs::OpenOptions::new()
                .write(true)
                .open(destination)
                .and_then(|file| file.sync_all())
                .map_err(|_| StoreError::Invalid("BACKUP_SYNC_FAILED".into()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }

    pub fn verify_integrity(&self) -> Result<(), StoreError> {
        let mut check = self.connection.prepare("PRAGMA integrity_check")?;
        let mut rows = check.query([])?;
        let mut ok = false;
        while let Some(row) = rows.next()? {
            if row.get::<_, String>(0)? != "ok" {
                return Err(StoreError::Invalid("ARCHIVE_INTEGRITY_FAILED".into()));
            }
            ok = true;
        }
        if !ok
            || self
                .connection
                .prepare("PRAGMA foreign_key_check")?
                .query([])?
                .next()?
                .is_some()
        {
            return Err(StoreError::Invalid("ARCHIVE_INTEGRITY_FAILED".into()));
        }
        self.summary()?;
        Ok(())
    }

    pub fn visit_verified_media(
        &self,
        mut visit: impl FnMut(&str, Option<u64>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT content_sha256, size_bytes FROM media WHERE integrity = 'verified'")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let size = row
                .get::<_, Option<i64>>(1)?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| StoreError::Invalid("BACKUP_MEDIA_INVALID".into()))?;
            visit(&row.get::<_, String>(0)?, size)?;
        }
        Ok(())
    }
}
