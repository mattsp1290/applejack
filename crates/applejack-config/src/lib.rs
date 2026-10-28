//! Per-game configuration database: the SQLite schema, migrations, presets,
//! and the resolve step that turns stored rows into launch settings. This is
//! the only SQLite handle in the workspace and only `applejack-core` depends
//! on it.

mod error;
mod migrations;

use std::path::Path;

use rusqlite::Connection;

pub use error::Error;

/// Handle to the configuration database. Wraps the workspace's only
/// `rusqlite::Connection`; every other crate goes through this type.
#[derive(Debug)]
pub struct ConfigDb {
    conn: Connection,
}

impl ConfigDb {
    /// Opens (creating if needed) the database at `path` in WAL mode and
    /// brings its schema up to date. Reopening an up-to-date database applies
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the parent directory cannot be created,
    /// [`Error::SchemaTooNew`] when the file was written by a newer build, and
    /// [`Error::Sqlite`] for any SQLite failure.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Self::prepare(Connection::open(path)?)
    }

    /// Opens a fresh in-memory database with the current schema.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when SQLite fails to open or migrate it.
    pub fn open_in_memory() -> Result<Self, Error> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(mut conn: Connection) -> Result<Self, Error> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrations::apply_pending(&mut conn)?;
        Ok(Self { conn })
    }

    /// The schema version recorded in `meta`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails or
    /// [`Error::InvalidSchemaVersion`] when the stored value is not a number.
    pub fn schema_version(&self) -> Result<u32, Error> {
        migrations::stored_version(&self.conn)
    }

    /// Names of all user tables, sorted, from `sqlite_master`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails.
    pub fn table_names(&self) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const EXPECTED_TABLES: [&str; 7] = [
        "apps",
        "d3dmetal",
        "engine_versions",
        "game_config",
        "meta",
        "presets",
        "sessions",
    ];

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "applejack-config-{label}-{}-{nanos}-{unique}",
                std::process::id()
            ));
            Self {
                path: dir.join("db.sqlite"),
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    #[test]
    fn open_on_temp_path_creates_all_tables() {
        let temp = TempDb::new("tables");
        let db = ConfigDb::open(&temp.path).unwrap();
        assert_eq!(db.table_names().unwrap(), EXPECTED_TABLES);
        assert_eq!(db.schema_version().unwrap(), migrations::LATEST_VERSION);
    }

    #[test]
    fn second_open_is_a_no_op() {
        let temp = TempDb::new("reopen");
        let first_version = {
            let db = ConfigDb::open(&temp.path).unwrap();
            db.conn
                .execute(
                    "INSERT INTO apps (appid, name, drm) VALUES (1, 'Test', 'steam')",
                    [],
                )
                .unwrap();
            db.schema_version().unwrap()
        };
        let db = ConfigDb::open(&temp.path).unwrap();
        assert_eq!(db.schema_version().unwrap(), first_version);
        assert_eq!(db.table_names().unwrap(), EXPECTED_TABLES);
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM apps", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "reopen must keep existing rows");
    }

    #[test]
    fn journal_mode_is_wal() {
        let temp = TempDb::new("wal");
        let db = ConfigDb::open(&temp.path).unwrap();
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        let timeout: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn bogus_drm_fails_check_constraint() {
        let db = ConfigDb::open_in_memory().unwrap();
        let result = db.conn.execute(
            "INSERT INTO apps (appid, name, drm) VALUES (1, 'Test', 'bogus')",
            [],
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("CHECK constraint failed"),
            "unexpected error: {error}"
        );
        for (appid, drm) in [(1, "unknown"), (2, "none"), (3, "steam")] {
            db.conn
                .execute(
                    "INSERT INTO apps (appid, drm) VALUES (?1, ?2)",
                    (appid, drm),
                )
                .unwrap();
        }
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let temp = TempDb::new("newer");
        {
            let db = ConfigDb::open(&temp.path).unwrap();
            db.conn
                .execute(
                    "UPDATE meta SET value = ?1 WHERE key = ?2",
                    (
                        (migrations::LATEST_VERSION + 1).to_string(),
                        migrations::SCHEMA_VERSION_KEY,
                    ),
                )
                .unwrap();
        }
        let error = ConfigDb::open(&temp.path).unwrap_err();
        assert!(matches!(error, Error::SchemaTooNew { .. }), "{error}");
    }
}
