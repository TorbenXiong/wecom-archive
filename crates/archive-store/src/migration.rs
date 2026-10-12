use crate::{ArchiveStore, StoreError};

use crate::schema::{MIGRATION_V2, PERFORMANCE_INDEXES, SCHEMA, SCHEMA_VERSION};

impl ArchiveStore {
    pub(super) fn migrate(&mut self) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(SCHEMA)?;
        let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::Invalid(format!(
                "archive schema {version} is newer than supported {SCHEMA_VERSION}"
            )));
        }
        if version == 1 {
            transaction.execute_batch(MIGRATION_V2)?;
        }
        transaction.execute_batch(PERFORMANCE_INDEXES)?;
        crate::derived::initialize(&transaction)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(())
    }
}
