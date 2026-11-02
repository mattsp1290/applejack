use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::Error;

pub(crate) const SCHEMA_VERSION_KEY: &str = "schema_version";

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("schema.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("schema_v2.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("schema_v3.sql"),
    },
    Migration {
        version: 4,
        sql: include_str!("schema_v4.sql"),
    },
];

pub(crate) const LATEST_VERSION: u32 = match MIGRATIONS.last() {
    Some(migration) => migration.version,
    None => 0,
};

/// Applies every migration above the stored version, one IMMEDIATE
/// transaction each, so a crash leaves the database at a whole version and
/// concurrent openers apply each migration exactly once.
pub(crate) fn apply_pending(conn: &mut Connection) -> Result<(), Error> {
    let current = check_supported(stored_version(conn)?)?;
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if check_supported(stored_version(&tx)?)? >= migration.version {
            continue;
        }
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (SCHEMA_VERSION_KEY, migration.version.to_string()),
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn check_supported(version: u32) -> Result<u32, Error> {
    if version > LATEST_VERSION {
        return Err(Error::SchemaTooNew {
            found: version,
            supported: LATEST_VERSION,
        });
    }
    Ok(version)
}

/// The version recorded in `meta`, or 0 when the database is empty. The
/// `meta` table itself is created by migration 1, so its absence is the
/// pre-migration state rather than an error.
pub(crate) fn stored_version(conn: &Connection) -> Result<u32, Error> {
    let has_meta: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [SCHEMA_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    match value {
        None => Ok(0),
        Some(text) => text
            .trim()
            .parse()
            .map_err(|_| Error::InvalidSchemaVersion(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_versions_are_contiguous_from_one() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(migration.version as usize, index + 1);
        }
    }

    /// A file at schema 2, as a binary of that version leaves it.
    fn at_schema_2() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS.iter().filter(|m| m.version <= 2) {
            conn.execute_batch(migration.sql).unwrap();
        }
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '2') \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION_KEY],
        )
        .unwrap();
        conn
    }

    #[test]
    fn schema_3_keeps_rows_of_schema_2_and_settles_old_sessions() {
        let mut conn = at_schema_2();
        conn.execute_batch(
            "INSERT INTO apps (appid, name) VALUES (10, 'kept');
             INSERT INTO sessions (appid, started_at, ended_at, exit_code)
                 VALUES (10, 'then', 'later', 0);
             INSERT INTO sessions (appid, started_at) VALUES (10, 'open one');
             INSERT INTO sessions (appid) VALUES (10);",
        )
        .unwrap();
        apply_pending(&mut conn).unwrap();
        assert_eq!(stored_version(&conn).unwrap(), LATEST_VERSION);
        let name: String = conn
            .query_row("SELECT name FROM apps WHERE appid = 10", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "kept");

        let db = crate::ConfigDb { conn };
        assert_eq!(db.open_sessions().unwrap(), []);
        let rows = db.sessions(Some(10)).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| row.state == crate::SessionState::Lost)
        );
        assert_eq!(rows[2].ended_at.as_deref(), Some("later"));
        assert_eq!(rows[2].exit_code, Some(0));
    }

    #[test]
    fn a_stored_version_that_is_not_a_number_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pending(&mut conn).unwrap();
        conn.execute(
            "UPDATE meta SET value = 'two' WHERE key = ?1",
            [SCHEMA_VERSION_KEY],
        )
        .unwrap();
        assert!(matches!(
            stored_version(&conn),
            Err(Error::InvalidSchemaVersion(text)) if text == "two"
        ));
    }
}
