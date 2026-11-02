//! Typed view of the `sessions` table. The states and who moves a row
//! between them are described by `applejack_core::runtime::session`.

use std::fmt;
use std::str::FromStr;

use rusqlite::{ErrorCode, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{ConfigDb, Error};

/// Where a session is in its life. The first two are open: the row has no
/// `ended_at` and a process owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Claimed: the prefix lock is held, the title has not been seen yet.
    Starting,
    /// The game-running marker appeared.
    Running,
    /// The marker cleared with nobody asking.
    Exited,
    /// The marker cleared after `app.cancel`.
    Cancelled,
    /// The title never ran; `note` says why.
    Failed,
    /// The owning core died and the title was gone when that was noticed;
    /// the exit code is unknown.
    Lost,
}

impl SessionState {
    /// Every value in declaration order; matches the column's CHECK constraint.
    pub const ALL: [Self; 6] = [
        Self::Starting,
        Self::Running,
        Self::Exited,
        Self::Cancelled,
        Self::Failed,
        Self::Lost,
    ];

    /// The name stored in the `state` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Lost => "lost",
        }
    }

    /// Whether the session has not ended.
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

impl fmt::Display for SessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionState {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == text)
            .ok_or_else(|| {
                "expected one of starting, running, exited, cancelled, failed, lost".to_owned()
            })
    }
}

/// One `sessions` row as `session.list` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRow {
    pub id: i64,
    pub appid: u32,
    /// When the row was claimed, then when the marker appeared.
    pub started_at: String,
    pub ended_at: Option<String>,
    pub engine_id: String,
    pub engine_version: String,
    /// The title's exit status (128 plus the signal). `NULL` when no process
    /// of this core was its parent.
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub state: SessionState,
    /// Value of `APPLEJACK_SESSION` in every process of the session.
    pub session_key: String,
    /// The prefix the session holds.
    pub prefix: String,
    /// File name the game-running marker looks for.
    pub launch_exe: String,
    pub note: Option<String>,
    pub cancel_requested: bool,
}

/// What is known when a session is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewSession<'a> {
    pub appid: u32,
    pub started_at: &'a str,
    pub engine_id: &'a str,
    pub engine_version: &'a str,
    pub log_path: &'a str,
    pub session_key: &'a str,
    pub prefix: &'a str,
    pub launch_exe: &'a str,
}

/// How a session closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionEnd<'a> {
    pub ended_at: &'a str,
    pub state: SessionState,
    pub exit_code: Option<i32>,
    pub note: Option<&'a str>,
}

const COLUMNS: &str = "id, appid, started_at, ended_at, engine_id, engine_version, exit_code, \
                       log_path, state, session_key, prefix, launch_exe, note, cancel_requested";

impl ConfigDb {
    /// Inserts an open row in [`SessionState::Starting`]. `None` when the
    /// title already has an open row, which another process may have written
    /// a moment ago.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] for any other write failure.
    pub fn insert_session(&self, new: &NewSession<'_>) -> Result<Option<i64>, Error> {
        self.write_locked(|db| {
            let inserted = db.conn.execute(
                "INSERT INTO sessions (appid, started_at, engine_id, engine_version, log_path, \
                 state, session_key, prefix, launch_exe) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    new.appid,
                    new.started_at,
                    new.engine_id,
                    new.engine_version,
                    new.log_path,
                    SessionState::Starting.as_str(),
                    new.session_key,
                    new.prefix,
                    new.launch_exe,
                ],
            );
            match inserted {
                Ok(_) => Ok(Some(db.conn.last_insert_rowid())),
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == ErrorCode::ConstraintViolation =>
                {
                    Ok(None)
                }
                Err(error) => Err(error.into()),
            }
        })
    }

    /// The row with this id, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`], or [`Error::InvalidValue`] for a row with
    /// an unknown state.
    pub fn session(&self, id: i64) -> Result<Option<SessionRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM sessions WHERE id = ?1"))?;
        stmt.query_row([id], read_session)
            .optional()?
            .map(RawSession::into_row)
            .transpose()
    }

    /// The title's open row, or `None`.
    ///
    /// # Errors
    ///
    /// Same as [`ConfigDb::session`].
    pub fn open_session(&self, appid: u32) -> Result<Option<SessionRow>, Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM sessions WHERE appid = ?1 AND ended_at IS NULL"
        ))?;
        stmt.query_row([appid], read_session)
            .optional()?
            .map(RawSession::into_row)
            .transpose()
    }

    /// Every open row, oldest first.
    ///
    /// # Errors
    ///
    /// Same as [`ConfigDb::session`].
    pub fn open_sessions(&self) -> Result<Vec<SessionRow>, Error> {
        self.collect_sessions(
            &format!("SELECT {COLUMNS} FROM sessions WHERE ended_at IS NULL ORDER BY id"),
            [],
        )
    }

    /// Past and open rows, newest first, of one title or of all.
    ///
    /// # Errors
    ///
    /// Same as [`ConfigDb::session`].
    pub fn sessions(&self, appid: Option<u32>) -> Result<Vec<SessionRow>, Error> {
        match appid {
            Some(appid) => self.collect_sessions(
                &format!("SELECT {COLUMNS} FROM sessions WHERE appid = ?1 ORDER BY id DESC"),
                [appid],
            ),
            None => self.collect_sessions(
                &format!("SELECT {COLUMNS} FROM sessions ORDER BY id DESC"),
                [],
            ),
        }
    }

    fn collect_sessions<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<SessionRow>, Error> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params, read_session)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(RawSession::into_row).collect()
    }

    /// Moves an open row to [`SessionState::Running`] and restamps its start.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn mark_session_running(&self, id: i64, at: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute(
                "UPDATE sessions SET state = ?1, started_at = ?2 \
                 WHERE id = ?3 AND ended_at IS NULL",
                params![SessionState::Running.as_str(), at, id],
            )?;
            Ok(())
        })
    }

    /// Records that `app.cancel` asked for this session to end; the end is
    /// then written as [`SessionState::Cancelled`] by whoever closes it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn request_session_cancel(&self, id: i64) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute(
                "UPDATE sessions SET cancel_requested = 1 WHERE id = ?1 AND ended_at IS NULL",
                [id],
            )?;
            Ok(())
        })
    }

    /// Closes an open row. A row already closed stays as it is, and the
    /// answer is `false`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn end_session(&self, id: i64, end: &SessionEnd<'_>) -> Result<bool, Error> {
        self.write_locked(|db| {
            let changed = db.conn.execute(
                "UPDATE sessions SET ended_at = ?1, state = ?2, exit_code = ?3, note = ?4 \
                 WHERE id = ?5 AND ended_at IS NULL",
                params![
                    end.ended_at,
                    end.state.as_str(),
                    end.exit_code,
                    end.note,
                    id
                ],
            )?;
            Ok(changed == 1)
        })
    }
}

struct RawSession {
    id: i64,
    appid: u32,
    started_at: Option<String>,
    ended_at: Option<String>,
    engine_id: Option<String>,
    engine_version: Option<String>,
    exit_code: Option<i32>,
    log_path: Option<String>,
    state: Option<String>,
    session_key: Option<String>,
    prefix: Option<String>,
    launch_exe: Option<String>,
    note: Option<String>,
    cancel_requested: bool,
}

fn read_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSession> {
    Ok(RawSession {
        id: row.get(0)?,
        appid: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        engine_id: row.get(4)?,
        engine_version: row.get(5)?,
        exit_code: row.get(6)?,
        log_path: row.get(7)?,
        state: row.get(8)?,
        session_key: row.get(9)?,
        prefix: row.get(10)?,
        launch_exe: row.get(11)?,
        note: row.get(12)?,
        cancel_requested: row.get(13)?,
    })
}

impl RawSession {
    fn into_row(self) -> Result<SessionRow, Error> {
        let text = self.state.unwrap_or_default();
        let state = text.parse().map_err(|reason| Error::InvalidValue {
            field: "state",
            value: text,
            reason,
        })?;
        Ok(SessionRow {
            id: self.id,
            appid: self.appid,
            started_at: self.started_at.unwrap_or_default(),
            ended_at: self.ended_at,
            engine_id: self.engine_id.unwrap_or_default(),
            engine_version: self.engine_version.unwrap_or_default(),
            exit_code: self.exit_code,
            log_path: self.log_path.unwrap_or_default(),
            state,
            session_key: self.session_key.unwrap_or_default(),
            prefix: self.prefix.unwrap_or_default(),
            launch_exe: self.launch_exe.unwrap_or_default(),
            note: self.note,
            cancel_requested: self.cancel_requested,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(appid: u32) -> NewSession<'static> {
        NewSession {
            appid,
            started_at: "2026-11-01T00:00:00Z",
            engine_id: "rosetta-wine",
            engine_version: "11.0-2",
            log_path: "/data/logs/k/wine.log",
            session_key: "launch-k",
            prefix: "10",
            launch_exe: "game.exe",
        }
    }

    const END: SessionEnd<'static> = SessionEnd {
        ended_at: "2026-11-01T01:00:00Z",
        state: SessionState::Exited,
        exit_code: Some(0),
        note: None,
    };

    #[test]
    fn a_title_has_one_open_session_and_a_closed_one_frees_it() {
        let db = ConfigDb::open_in_memory().unwrap();
        let id = db.insert_session(&new(10)).unwrap().unwrap();
        assert_eq!(db.insert_session(&new(10)).unwrap(), None);
        assert!(db.insert_session(&new(11)).unwrap().is_some());
        assert!(db.end_session(id, &END).unwrap());
        assert!(db.open_session(10).unwrap().is_none());
        assert!(db.insert_session(&new(10)).unwrap().is_some());
    }

    #[test]
    fn a_session_moves_from_starting_to_running_to_an_end_once() {
        let db = ConfigDb::open_in_memory().unwrap();
        let id = db.insert_session(&new(10)).unwrap().unwrap();
        let row = db.open_session(10).unwrap().unwrap();
        assert_eq!(row.state, SessionState::Starting);
        assert!(row.state.is_open());
        assert!(!row.cancel_requested);

        db.mark_session_running(id, "2026-11-01T00:00:05Z").unwrap();
        db.request_session_cancel(id).unwrap();
        let row = db.session(id).unwrap().unwrap();
        assert_eq!(row.state, SessionState::Running);
        assert_eq!(row.started_at, "2026-11-01T00:00:05Z");
        assert!(row.cancel_requested);

        assert!(db.end_session(id, &END).unwrap());
        let closed = SessionEnd {
            state: SessionState::Lost,
            ..END
        };
        assert!(!db.end_session(id, &closed).unwrap());
        let row = db.session(id).unwrap().unwrap();
        assert_eq!(row.state, SessionState::Exited);
        assert_eq!(row.exit_code, Some(0));
        assert!(!row.state.is_open());
    }

    #[test]
    fn lists_are_newest_first_and_filter_by_title() {
        let db = ConfigDb::open_in_memory().unwrap();
        let first = db.insert_session(&new(10)).unwrap().unwrap();
        db.end_session(first, &END).unwrap();
        let second = db.insert_session(&new(10)).unwrap().unwrap();
        let other = db.insert_session(&new(11)).unwrap().unwrap();
        let ids = |rows: Vec<SessionRow>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(
            ids(db.sessions(None).unwrap()),
            [other, second, first],
            "newest first"
        );
        assert_eq!(ids(db.sessions(Some(10)).unwrap()), [second, first]);
        assert_eq!(ids(db.open_sessions().unwrap()), [second, other]);
    }

    #[test]
    fn every_state_round_trips_through_its_name() {
        for state in SessionState::ALL {
            assert_eq!(state.as_str().parse::<SessionState>(), Ok(state));
        }
        assert!("paused".parse::<SessionState>().is_err());
    }
}
