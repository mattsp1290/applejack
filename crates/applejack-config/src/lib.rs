//! Per-game configuration database: the SQLite schema, migrations, presets,
//! and the resolve step that turns stored rows into launch settings. This is
//! the only SQLite handle in the workspace and only `applejack-core` depends
//! on it.

mod apps;
mod error;
mod game_config;
mod migrations;
pub mod presets;
pub mod resolve;

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

pub use apps::{AppRow, Drm};
pub use error::Error;
pub use game_config::{Field, GameConfig, GraphicsBackend, LaunchMode, SyncMode};
pub use presets::{EnvPolicy, ImportOptions, ImportPlan, Preset, ValueCheck};
pub use resolve::{Resolved, resolve};

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
const WAL_RETRY_INTERVAL: Duration = Duration::from_millis(10);

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
    /// [`Error::JournalMode`] when SQLite will not switch the file to WAL,
    /// [`Error::SchemaTooNew`] when the file was written by a newer build, and
    /// [`Error::Sqlite`] for any SQLite failure.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Self::prepare(Connection::open(path)?, "wal")
    }

    /// Opens a fresh in-memory database with the current schema.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when SQLite fails to open or migrate it.
    pub fn open_in_memory() -> Result<Self, Error> {
        Self::prepare(Connection::open_in_memory()?, "memory")
    }

    fn prepare(mut conn: Connection, expected_mode: &str) -> Result<Self, Error> {
        let deadline = Instant::now() + BUSY_TIMEOUT;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode = enable_wal(&conn, deadline)?;
        if !mode.eq_ignore_ascii_case(expected_mode) {
            return Err(Error::JournalMode { found: mode });
        }
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

    /// Whether a `D3DMetal` overlay has been accepted for `engine_version`,
    /// per the `d3dmetal` table.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails.
    pub fn d3dmetal_accepted(&self, engine_version: &str) -> Result<bool, Error> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM d3dmetal WHERE engine_version = ?1",
            [engine_version],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Records that the user accepted a `D3DMetal` overlay for
    /// `engine_version`, so `graphics_backend = d3dmetal` imports for it.
    /// `accepted_at` is the caller's timestamp text; this crate never reads
    /// the clock. Accepting again keeps the row and updates the stamp.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn d3dmetal_accept(&self, engine_version: &str, accepted_at: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO d3dmetal (engine_version, accepted_at) VALUES (?1, ?2) \
             ON CONFLICT(engine_version) DO UPDATE SET accepted_at = excluded.accepted_at",
            [engine_version, accepted_at],
        )?;
        Ok(())
    }

    /// Runs `f` inside a `BEGIN IMMEDIATE` transaction, so a read-modify-write
    /// of one row cannot interleave with another connection's write: the
    /// write lock is taken before the read, and a second writer waits on the
    /// busy timeout instead of overwriting with a stale copy. An error from
    /// `f` rolls the transaction back. Called with a transaction already
    /// open on this connection, it runs `f` inside a savepoint of that
    /// transaction, so an inner error the outer closure swallows still undoes
    /// the inner writes.
    ///
    /// Every transaction opened on this connection must be IMMEDIATE: nesting
    /// is detected with `is_autocommit`, and a DEFERRED transaction that has
    /// already read would lose the write-before-read guarantee (in WAL mode
    /// the inner write then fails with `SQLITE_BUSY_SNAPSHOT`).
    pub(crate) fn write_locked<T>(
        &self,
        f: impl FnOnce(&Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("SAVEPOINT write_locked")?;
            return match f(self) {
                Ok(value) => {
                    self.conn.execute_batch("RELEASE write_locked")?;
                    Ok(value)
                }
                Err(error) => {
                    self.conn
                        .execute_batch("ROLLBACK TO write_locked; RELEASE write_locked")?;
                    Err(error)
                }
            };
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let value = f(self)?;
        tx.commit()?;
        Ok(value)
    }
}

/// Switches to WAL and returns the journal mode SQLite reports afterwards.
fn enable_wal(conn: &Connection, deadline: Instant) -> Result<String, rusqlite::Error> {
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0)) {
            // The WAL switch can return SQLITE_BUSY without consulting the busy
            // handler while another connection creates the file, so retry it here.
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(WAL_RETRY_INTERVAL);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Barrier;
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

    #[test]
    fn concurrent_open_of_fresh_path_succeeds() {
        const THREADS: usize = 4;
        for round in 0..20 {
            let temp = TempDb::new("concurrent");
            let barrier = Barrier::new(THREADS);
            let results: Vec<Result<u32, Error>> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            ConfigDb::open(&temp.path)?.schema_version()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap())
                    .collect()
            });
            for result in results {
                match result {
                    Ok(version) => assert_eq!(version, migrations::LATEST_VERSION),
                    Err(error) => panic!("round {round}: open failed: {error}"),
                }
            }
        }
    }

    #[test]
    fn concurrent_set_field_on_two_connections_keeps_every_entry() {
        const WRITES_PER_THREAD: usize = 20;
        let temp = TempDb::new("race");
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|thread| {
                    let barrier = &barrier;
                    let path = &temp.path;
                    scope.spawn(move || {
                        let db = ConfigDb::open(path)?;
                        barrier.wait();
                        for i in 0..WRITES_PER_THREAD {
                            db.set_field(40, "extra_env", &format!("WINEDEBUG_{thread}_{i}=x"))?;
                        }
                        Ok::<(), Error>(())
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap().unwrap();
            }
        });
        let db = ConfigDb::open(&temp.path).unwrap();
        let config = db.game_config(40).unwrap().unwrap();
        assert_eq!(
            config.extra_env.len(),
            2 * WRITES_PER_THREAD,
            "lost updates: {:?}",
            config.extra_env.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn d3dmetal_accepted_reads_the_table() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert!(!db.d3dmetal_accepted("1.0.0").unwrap());
        db.conn
            .execute(
                "INSERT INTO d3dmetal (engine_version, accepted_at) VALUES ('1.0.0', 'now')",
                [],
            )
            .unwrap();
        assert!(db.d3dmetal_accepted("1.0.0").unwrap());
        assert!(!db.d3dmetal_accepted("1.0.1").unwrap());
        db.d3dmetal_accept("1.0.1", "2026-10-29T00:00:00Z").unwrap();
        db.d3dmetal_accept("1.0.1", "2026-10-30T00:00:00Z").unwrap();
        assert!(db.d3dmetal_accepted("1.0.1").unwrap());
        let stamp: String = db
            .conn
            .query_row(
                "SELECT accepted_at FROM d3dmetal WHERE engine_version = '1.0.1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stamp, "2026-10-30T00:00:00Z");
    }

    #[test]
    fn write_locked_rolls_back_on_error_and_nests_inside_an_open_transaction() {
        let db = ConfigDb::open_in_memory().unwrap();
        let error = db
            .write_locked(|db| {
                db.meta_set("k", "v")?;
                Err::<(), _>(Error::InvalidSchemaVersion("x".to_owned()))
            })
            .unwrap_err();
        assert!(matches!(error, Error::InvalidSchemaVersion(_)), "{error}");
        assert_eq!(db.meta_get("k").unwrap(), None, "rolled back");
        db.write_locked(|db| {
            db.meta_set("k", "outer")?;
            db.write_locked(|db| db.meta_set("k2", "inner"))
        })
        .unwrap();
        assert_eq!(db.meta_get("k").unwrap().as_deref(), Some("outer"));
        assert_eq!(db.meta_get("k2").unwrap().as_deref(), Some("inner"));
        assert!(db.conn.is_autocommit(), "transaction closed");
    }

    #[test]
    fn nested_write_locked_undoes_an_inner_error_the_outer_closure_swallows() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.write_locked(|db| {
            db.meta_set("k", "outer")?;
            let inner = db.write_locked(|db| {
                db.meta_set("k2", "inner")?;
                Err::<(), _>(Error::InvalidSchemaVersion("x".to_owned()))
            });
            assert!(inner.is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(db.meta_get("k").unwrap().as_deref(), Some("outer"));
        assert_eq!(db.meta_get("k2").unwrap(), None, "inner write rolled back");
        assert!(db.conn.is_autocommit(), "transaction closed");
    }

    #[test]
    fn null_text_keys_fail_not_null_constraint() {
        let db = ConfigDb::open_in_memory().unwrap();
        for sql in [
            "INSERT INTO engine_versions (engine_id, version) VALUES (NULL, '1')",
            "INSERT INTO engine_versions (engine_id, version) VALUES ('engine', NULL)",
            "INSERT INTO d3dmetal (engine_version) VALUES (NULL)",
            "INSERT INTO meta (key, value) VALUES (NULL, 'value')",
        ] {
            let error = db.conn.execute(sql, []).unwrap_err();
            assert!(
                error.to_string().contains("NOT NULL constraint failed"),
                "{sql}: unexpected error: {error}"
            );
        }
    }
}
