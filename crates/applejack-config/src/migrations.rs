use rusqlite::{Connection, OptionalExtension};

use crate::Error;

pub(crate) const SCHEMA_VERSION_KEY: &str = "schema_version";

struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("schema.sql"),
}];

pub(crate) const LATEST_VERSION: u32 = match MIGRATIONS.last() {
    Some(migration) => migration.version,
    None => 0,
};

/// Applies every migration above the stored version, one transaction each,
/// so a crash leaves the database at a whole version.
pub(crate) fn apply_pending(conn: &mut Connection) -> Result<(), Error> {
    let current = stored_version(conn)?;
    if current > LATEST_VERSION {
        return Err(Error::SchemaTooNew {
            found: current,
            supported: LATEST_VERSION,
        });
    }
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
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
