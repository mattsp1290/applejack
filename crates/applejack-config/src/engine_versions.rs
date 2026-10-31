//! `engine_versions`: which engine versions were installed and which one is
//! active. The bundles themselves live on disk under `engines/`; this table
//! is the only record of the active version.

use rusqlite::OptionalExtension;

use crate::{ConfigDb, Error};

/// One `engine_versions` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineVersionRow {
    pub engine_id: String,
    pub version: String,
    pub installed_at: Option<String>,
    pub active: bool,
}

impl ConfigDb {
    /// The rows of `engine_id`, by version text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails.
    pub fn engine_versions(&self, engine_id: &str) -> Result<Vec<EngineVersionRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT engine_id, version, installed_at, COALESCE(active, 0) \
             FROM engine_versions WHERE engine_id = ?1 ORDER BY version",
        )?;
        let rows = stmt
            .query_map([engine_id], |row| {
                Ok(EngineVersionRow {
                    engine_id: row.get(0)?,
                    version: row.get(1)?,
                    installed_at: row.get(2)?,
                    active: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The active version of `engine_id`, when one is selected.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails.
    pub fn active_engine_version(&self, engine_id: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT version FROM engine_versions WHERE engine_id = ?1 AND active = 1",
                [engine_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records an installed version and makes it active when `engine_id`
    /// has no active version, so a first install needs no `engine use`.
    /// `installed_at` is the caller's timestamp text. Returns whether the
    /// version is active afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn record_engine_version(
        &self,
        engine_id: &str,
        version: &str,
        installed_at: &str,
    ) -> Result<bool, Error> {
        self.write_locked(|db| {
            db.conn.execute(
                "INSERT INTO engine_versions (engine_id, version, installed_at, active) \
                 VALUES (?1, ?2, ?3, 0) \
                 ON CONFLICT(engine_id, version) DO UPDATE SET installed_at = excluded.installed_at",
                [engine_id, version, installed_at],
            )?;
            if let Some(active) = db.active_engine_version(engine_id)? {
                return Ok(active == version);
            }
            db.activate_engine_version(engine_id, version)?;
            Ok(true)
        })
    }

    /// Makes `version` the only active version of `engine_id`, adding its
    /// row when the bundle was installed without one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn activate_engine_version(&self, engine_id: &str, version: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute(
                "UPDATE engine_versions SET active = 0 WHERE engine_id = ?1 AND active = 1",
                [engine_id],
            )?;
            db.conn.execute(
                "INSERT INTO engine_versions (engine_id, version, active) VALUES (?1, ?2, 1) \
                 ON CONFLICT(engine_id, version) DO UPDATE SET active = 1",
                [engine_id, version],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_recorded_version_turns_active_and_a_later_one_does_not() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert_eq!(db.active_engine_version("e").unwrap(), None);
        assert!(db.record_engine_version("e", "1", "t1").unwrap());
        assert!(!db.record_engine_version("e", "2", "t2").unwrap());
        assert_eq!(db.active_engine_version("e").unwrap().as_deref(), Some("1"));
        assert!(db.record_engine_version("e", "1", "t3").unwrap());
        let rows = db.engine_versions("e").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].installed_at.as_deref(), Some("t3"));
        assert!(rows[0].active);
        assert!(!rows[1].active);
    }

    #[test]
    fn activation_moves_and_another_engine_keeps_its_own() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.record_engine_version("e", "1", "t").unwrap();
        db.record_engine_version("other", "1", "t").unwrap();
        db.activate_engine_version("e", "2").unwrap();
        assert_eq!(db.active_engine_version("e").unwrap().as_deref(), Some("2"));
        assert_eq!(
            db.active_engine_version("other").unwrap().as_deref(),
            Some("1")
        );
        let active = db
            .engine_versions("e")
            .unwrap()
            .into_iter()
            .filter(|row| row.active)
            .count();
        assert_eq!(active, 1);
    }

    #[test]
    fn the_index_refuses_a_second_active_row() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.activate_engine_version("e", "1").unwrap();
        let second = db.conn.execute(
            "INSERT INTO engine_versions (engine_id, version, active) VALUES ('e', '2', 1)",
            [],
        );
        assert!(second.is_err());
    }

    #[test]
    fn d3dmetal_forget_clears_the_acceptance() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.d3dmetal_accept("1", "/Volumes/x", "3.0", "t").unwrap();
        assert!(db.d3dmetal_accepted("1").unwrap());
        db.d3dmetal_forget("1").unwrap();
        assert!(!db.d3dmetal_accepted("1").unwrap());
    }
}
