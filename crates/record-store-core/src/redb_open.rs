//! Opening a redb database, and refusing an older file format clearly.

use std::path::Path;

use redb::{Database, DatabaseError, StorageError};

/// The release that migrates a v2 database to v3. Named in the error below
/// because the file format is not something an operator can act on directly.
const MIGRATION_RELEASE: &str = "0.1.3";

/// Opens a redb database at `path`, creating it if absent.
///
/// redb 4 reads only file format v3. Record Store wrote v2 up to and including
/// 0.1.2, and 0.1.3 migrates a v2 file to v3 when it opens it. A deployment
/// that upgraded straight from 0.1.2 to this release therefore still has v2 on
/// disk, and redb refuses it with a message about a file format the operator
/// never chose. This turns that into the instruction that resolves it.
///
/// The migration cannot happen here: `Database::upgrade` exists only in redb
/// 2.6, and redb 4 removed it along with the ability to read v2.
pub fn open_database(path: impl AsRef<Path>) -> Result<Database, DatabaseError> {
    let path = path.as_ref();
    match Database::create(path) {
        Err(DatabaseError::UpgradeRequired(version)) => {
            Err(DatabaseError::Storage(StorageError::Corrupted(format!(
                "{} is in redb file format v{version}, which this release cannot read. \
                 Upgrade to Record Store {MIGRATION_RELEASE} first and start it once — it \
                 converts the file to v3 — then upgrade to this release again. Restore from \
                 backup if {MIGRATION_RELEASE} was skipped and the file has since been written to.",
                path.display()
            ))))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use redb::{ReadableDatabase, TableDefinition};
    use tempfile::tempdir;

    use super::*;

    const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("test.v1");

    #[test]
    fn a_database_round_trips_through_open() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("store.redb");

        let database = open_database(&path).expect("create");
        let write = database.begin_write().expect("begin write");
        {
            let mut table = write.open_table(TABLE).expect("open table");
            table.insert("key", b"value".as_slice()).expect("insert");
        }
        write.commit().expect("commit");
        drop(database);

        let database = open_database(&path).expect("reopen");
        let read = database.begin_read().expect("begin read");
        let table = read.open_table(TABLE).expect("open table");
        assert_eq!(
            table
                .get("key")
                .expect("get")
                .map(|value| value.value().to_vec()),
            Some(b"value".to_vec())
        );
    }

    // The path that matters: a database written by 0.1.2, which this release
    // cannot read. Built with the redb version that still writes v2, so the
    // refusal is exercised rather than described.
    #[test]
    fn an_old_file_format_names_the_release_that_migrates_it() {
        const LEGACY: redb_v2::TableDefinition<&str, &[u8]> =
            redb_v2::TableDefinition::new("test.v1");

        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("metadata.redb");

        let legacy = redb_v2::Database::create(&path).expect("write a v2 database");
        let write = legacy.begin_write().expect("begin write");
        {
            let mut table = write.open_table(LEGACY).expect("open table");
            table.insert("key", b"value".as_slice()).expect("insert");
        }
        write.commit().expect("commit");
        drop(legacy);

        let error = open_database(&path).expect_err("a v2 file must be refused");
        let message = error.to_string();
        assert!(
            message.contains(MIGRATION_RELEASE),
            "the refusal must name the release that converts the file: {message}"
        );
        assert!(
            message.contains("metadata.redb"),
            "the refusal must name the file: {message}"
        );
    }
}
