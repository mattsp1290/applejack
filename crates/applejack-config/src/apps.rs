//! Typed view of the `apps` table and the `meta` key-value table.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{ConfigDb, Error};

/// Whether a title needs the Windows Steam client at launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Drm {
    /// Not probed yet; takes the client launch path.
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
    /// Verified to run without a Steam client.
    #[serde(rename = "none")]
    None,
    /// Steam DRM; launches through the client.
    #[serde(rename = "steam")]
    Steam,
}

impl Drm {
    /// Every value in declaration order; matches the table's CHECK constraint.
    pub const ALL: [Self; 3] = [Self::Unknown, Self::None, Self::Steam];

    /// The name stored in the `drm` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::Steam => "steam",
        }
    }
}

impl fmt::Display for Drm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Drm {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == text)
            .ok_or_else(|| "expected one of unknown, none, steam".to_owned())
    }
}

/// One `apps` row: what the launcher knows about an installed or fetched title.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppRow {
    pub appid: u32,
    pub name: Option<String>,
    pub installdir: Option<String>,
    pub drm: Drm,
    pub library_dir: Option<String>,
    pub last_installed_buildid: Option<u64>,
    /// Depot id to manifest id of the last completed install, as decimal text
    /// so a manifest id above `2^53` survives JSON.
    pub last_manifest_ids: BTreeMap<String, String>,
    /// When the library cache last saw this title; `None` for a title known
    /// only from disk.
    pub fetched_at: Option<String>,
    /// Bytes of the Windows depots on the default branch.
    pub size_bytes: Option<u64>,
    pub default_branch: Option<String>,
    /// The build Steam lists for `default_branch`.
    pub latest_buildid: Option<u64>,
    /// Relative to the install directory, from the app info's launch entries.
    /// `game_config.launch_exe_override` wins over it.
    pub launch_exe: Option<String>,
    pub launch_args: Option<String>,
    /// The branch on disk. `None` for a title installed elsewhere, which an
    /// update treats as the default branch.
    pub installed_branch: Option<String>,
}

/// One owned title as the library cache stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRow {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    pub size_bytes: u64,
    pub default_branch: String,
    pub latest_buildid: Option<u64>,
}

const APP_COLUMNS: &str = "appid, name, installdir, drm, library_dir, last_installed_buildid, last_manifest_ids, fetched_at, size_bytes, default_branch, latest_buildid, launch_exe, launch_args, installed_branch";

impl ConfigDb {
    /// Inserts or replaces the row for `row.appid`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails, [`Error::Json`] when
    /// `last_manifest_ids` cannot be encoded, and [`Error::InvalidValue`] when
    /// `last_installed_buildid` exceeds `i64::MAX`.
    pub fn upsert_app(&self, row: &AppRow) -> Result<(), Error> {
        let manifest_ids = serde_json::to_string(&row.last_manifest_ids)?;
        let buildid = integer("last_installed_buildid", row.last_installed_buildid)?;
        let size = integer("size_bytes", row.size_bytes)?;
        let latest = integer("latest_buildid", row.latest_buildid)?;
        self.write_locked(|db| {
            db.conn.execute(
                &format!(
                    "INSERT INTO apps ({APP_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
                     ON CONFLICT(appid) DO UPDATE SET name = excluded.name, \
                     installdir = excluded.installdir, drm = excluded.drm, \
                     library_dir = excluded.library_dir, \
                     last_installed_buildid = excluded.last_installed_buildid, \
                     last_manifest_ids = excluded.last_manifest_ids, \
                     fetched_at = excluded.fetched_at, \
                     size_bytes = excluded.size_bytes, \
                     default_branch = excluded.default_branch, \
                     latest_buildid = excluded.latest_buildid, \
                     launch_exe = excluded.launch_exe, \
                     launch_args = excluded.launch_args, \
                     installed_branch = excluded.installed_branch"
                ),
                params![
                    row.appid,
                    row.name,
                    row.installdir,
                    row.drm.as_str(),
                    row.library_dir,
                    buildid,
                    manifest_ids,
                    row.fetched_at,
                    size,
                    row.default_branch,
                    latest,
                    row.launch_exe,
                    row.launch_args,
                    row.installed_branch,
                ],
            )?;
            Ok(())
        })
    }

    /// Replaces the library cache with `rows`, in one transaction. What is
    /// on disk stays as it is: a row with a `library_dir` keeps its
    /// `installdir`, however Steam now spells it, and stays when the title
    /// leaves the account. `drm` and the launch columns are never touched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when a write fails and
    /// [`Error::InvalidValue`] for a number the column cannot hold.
    pub fn cache_library(&self, rows: &[LibraryRow], fetched_at: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute("UPDATE apps SET fetched_at = NULL", [])?;
            for row in rows {
                db.conn.execute(
                    "INSERT INTO apps (appid, name, installdir, drm, last_manifest_ids, \
                     fetched_at, size_bytes, default_branch, latest_buildid) \
                     VALUES (?1, ?2, ?3, 'unknown', '{}', ?4, ?5, ?6, ?7) \
                     ON CONFLICT(appid) DO UPDATE SET name = excluded.name, \
                     installdir = CASE WHEN apps.library_dir IS NULL \
                     THEN excluded.installdir ELSE apps.installdir END, \
                     fetched_at = excluded.fetched_at, \
                     size_bytes = excluded.size_bytes, \
                     default_branch = excluded.default_branch, \
                     latest_buildid = excluded.latest_buildid",
                    params![
                        row.appid,
                        row.name,
                        row.installdir,
                        fetched_at,
                        integer("size_bytes", Some(row.size_bytes))?,
                        row.default_branch,
                        integer("latest_buildid", row.latest_buildid)?,
                    ],
                )?;
            }
            db.conn.execute(
                "DELETE FROM apps WHERE library_dir IS NULL AND fetched_at IS NULL",
                [],
            )?;
            Ok(())
        })
    }

    /// [`cache_library`](Self::cache_library), only while `meta` still names
    /// `account` under `key`: a fetch can outlast a logout, and the check and
    /// the write must be one transaction for that to stay undone. `false`
    /// when the rows were dropped.
    ///
    /// # Errors
    ///
    /// The errors of [`cache_library`](Self::cache_library).
    pub fn cache_library_for(
        &self,
        key: &str,
        account: &str,
        rows: &[LibraryRow],
        fetched_at: &str,
    ) -> Result<bool, Error> {
        self.write_locked(|db| {
            if db.meta_get(key)?.as_deref() != Some(account) {
                return Ok(false);
            }
            db.cache_library(rows, fetched_at)?;
            Ok(true)
        })
    }

    /// Clears the library cache and the `meta` value under `key`, in one
    /// transaction, so [`cache_library_for`](Self::cache_library_for) sees
    /// both or neither.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when a write fails.
    pub fn forget_account(&self, key: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.clear_library_cache()?;
            db.meta_delete(key)
        })
    }

    /// Forgets what the account owns. Titles on disk stay, with their `drm`
    /// and launch columns: a logout does not uninstall anything.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when a write fails.
    pub fn clear_library_cache(&self) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn
                .execute("DELETE FROM apps WHERE library_dir IS NULL", [])?;
            db.conn.execute(
                "UPDATE apps SET fetched_at = NULL, size_bytes = NULL, \
                 default_branch = NULL, latest_buildid = NULL",
                [],
            )?;
            Ok(())
        })
    }

    /// The library cache, in appid order.
    ///
    /// # Errors
    ///
    /// The read errors of [`ConfigDb::apps`].
    pub fn cached_library(&self) -> Result<Vec<AppRow>, Error> {
        let mut rows = self.apps()?;
        rows.retain(|row| row.fetched_at.is_some());
        Ok(rows)
    }

    /// Sets a title's `drm`, the user's word that it runs without a client.
    /// `false` when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn set_app_drm(&self, appid: u32, drm: Drm) -> Result<bool, Error> {
        self.write_locked(|db| {
            let changed = db.conn.execute(
                "UPDATE apps SET drm = ?2 WHERE appid = ?1",
                params![appid, drm.as_str()],
            )?;
            Ok(changed > 0)
        })
    }

    /// The row for `appid`, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails, [`Error::Json`] when
    /// `last_manifest_ids` is not a JSON object, and [`Error::InvalidValue`]
    /// when `drm` holds an unknown name or the build id is negative.
    pub fn app(&self, appid: u32) -> Result<Option<AppRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {APP_COLUMNS} FROM apps WHERE appid = ?1"))?;
        stmt.query_row([appid], read_app)
            .optional()?
            .map(RawApp::into_row)
            .transpose()
    }

    /// Every row, ordered by `appid`.
    ///
    /// # Errors
    ///
    /// Same as [`ConfigDb::app`].
    pub fn apps(&self) -> Result<Vec<AppRow>, Error> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {APP_COLUMNS} FROM apps ORDER BY appid"))?;
        let rows = stmt
            .query_map([], read_app)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(RawApp::into_row).collect()
    }

    /// The `meta` value for `key`, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>, Error> {
        let value = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?;
        Ok(value.flatten())
    }

    /// Sets the `meta` value for `key`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )?;
            Ok(())
        })
    }

    /// Removes one `meta` key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails.
    pub fn meta_delete(&self, key: &str) -> Result<(), Error> {
        self.write_locked(|db| {
            db.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
            Ok(())
        })
    }
}

/// rusqlite's u64 conversion is behind a feature flag, and a value above
/// `i64::MAX` cannot be stored in an INTEGER column anyway.
fn integer(field: &'static str, value: Option<u64>) -> Result<Option<i64>, Error> {
    value
        .map(i64::try_from)
        .transpose()
        .map_err(|_| Error::InvalidValue {
            field,
            value: value.unwrap_or_default().to_string(),
            reason: "exceeds the INTEGER column range".to_owned(),
        })
}

fn unsigned(field: &'static str, value: Option<i64>) -> Result<Option<u64>, Error> {
    value
        .map(u64::try_from)
        .transpose()
        .map_err(|_| Error::InvalidValue {
            field,
            value: value.unwrap_or_default().to_string(),
            reason: "negative".to_owned(),
        })
}

struct RawApp {
    appid: u32,
    name: Option<String>,
    installdir: Option<String>,
    drm: Option<String>,
    library_dir: Option<String>,
    last_installed_buildid: Option<i64>,
    last_manifest_ids: Option<String>,
    fetched_at: Option<String>,
    size_bytes: Option<i64>,
    default_branch: Option<String>,
    latest_buildid: Option<i64>,
    launch_exe: Option<String>,
    launch_args: Option<String>,
    installed_branch: Option<String>,
}

fn read_app(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawApp> {
    Ok(RawApp {
        appid: row.get(0)?,
        name: row.get(1)?,
        installdir: row.get(2)?,
        drm: row.get(3)?,
        library_dir: row.get(4)?,
        last_installed_buildid: row.get(5)?,
        last_manifest_ids: row.get(6)?,
        fetched_at: row.get(7)?,
        size_bytes: row.get(8)?,
        default_branch: row.get(9)?,
        latest_buildid: row.get(10)?,
        launch_exe: row.get(11)?,
        launch_args: row.get(12)?,
        installed_branch: row.get(13)?,
    })
}

impl RawApp {
    fn into_row(self) -> Result<AppRow, Error> {
        let drm = match self.drm {
            None => Drm::Unknown,
            Some(text) => text.parse().map_err(|reason| Error::InvalidValue {
                field: "drm",
                value: text,
                reason,
            })?,
        };
        let last_manifest_ids = match self.last_manifest_ids.as_deref().map(str::trim) {
            None | Some("") => BTreeMap::new(),
            Some(json) => serde_json::from_str(json)?,
        };
        Ok(AppRow {
            appid: self.appid,
            name: self.name,
            installdir: self.installdir,
            drm,
            library_dir: self.library_dir,
            last_installed_buildid: unsigned(
                "last_installed_buildid",
                self.last_installed_buildid,
            )?,
            last_manifest_ids,
            fetched_at: self.fetched_at,
            size_bytes: unsigned("size_bytes", self.size_bytes)?,
            default_branch: self.default_branch,
            latest_buildid: unsigned("latest_buildid", self.latest_buildid)?,
            launch_exe: self.launch_exe,
            launch_args: self.launch_args,
            installed_branch: self.installed_branch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_rows_round_trip_and_list_in_appid_order() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert_eq!(db.app(10).unwrap(), None);
        let row = AppRow {
            appid: 10,
            name: Some("Example".to_owned()),
            installdir: Some("Example".to_owned()),
            drm: Drm::Steam,
            library_dir: Some("/data/library".to_owned()),
            last_installed_buildid: Some(123_456),
            last_manifest_ids: BTreeMap::from([("11".to_owned(), "9007199254740993".to_owned())]),
            fetched_at: Some("2026-10-28T00:00:00Z".to_owned()),
            size_bytes: Some(4096),
            default_branch: Some("public".to_owned()),
            latest_buildid: Some(123_457),
            launch_exe: Some("bin/game.exe".to_owned()),
            launch_args: Some("-windowed".to_owned()),
            installed_branch: Some("beta".to_owned()),
        };
        db.upsert_app(&row).unwrap();
        db.upsert_app(&AppRow {
            appid: 2,
            ..AppRow::default()
        })
        .unwrap();
        assert_eq!(db.app(10).unwrap(), Some(row.clone()));
        let renamed = AppRow {
            name: Some("Renamed".to_owned()),
            ..row
        };
        db.upsert_app(&renamed).unwrap();
        let ids: Vec<u32> = db.apps().unwrap().iter().map(|r| r.appid).collect();
        assert_eq!(ids, [2, 10]);
        assert_eq!(
            db.app(10).unwrap().unwrap().name.as_deref(),
            Some("Renamed")
        );
        assert_eq!(db.app(2).unwrap().unwrap().drm, Drm::Unknown);
    }

    fn owned(appid: u32, installdir: &str) -> LibraryRow {
        LibraryRow {
            appid,
            name: format!("Title {appid}"),
            installdir: installdir.to_owned(),
            size_bytes: 10,
            default_branch: "public".to_owned(),
            latest_buildid: Some(7),
        }
    }

    fn installed(appid: u32) -> AppRow {
        AppRow {
            appid,
            installdir: Some("OnDisk".to_owned()),
            drm: Drm::None,
            library_dir: Some("/data/library".to_owned()),
            last_installed_buildid: Some(5),
            launch_exe: Some("game.exe".to_owned()),
            ..AppRow::default()
        }
    }

    #[test]
    fn a_refresh_replaces_the_cache_and_leaves_what_is_on_disk() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.upsert_app(&installed(10)).unwrap();
        db.upsert_app(&installed(11)).unwrap();
        db.cache_library(
            &[owned(10, "Respelled"), owned(11, "Same"), owned(12, "Gone")],
            "t1",
        )
        .unwrap();
        db.set_app_drm(12, Drm::Steam).unwrap();
        db.cache_library(&[owned(10, "Respelled"), owned(13, "New")], "t2")
            .unwrap();

        let kept = db.app(10).unwrap().unwrap();
        assert_eq!(kept.installdir.as_deref(), Some("OnDisk"));
        assert_eq!(kept.drm, Drm::None);
        assert_eq!(kept.launch_exe.as_deref(), Some("game.exe"));
        assert_eq!(kept.latest_buildid, Some(7));
        assert_eq!(kept.fetched_at.as_deref(), Some("t2"));

        let left = db.app(11).unwrap().unwrap();
        assert_eq!(left.fetched_at, None, "installed but no longer owned");
        assert_eq!(left.last_installed_buildid, Some(5));
        assert_eq!(
            db.app(12).unwrap(),
            None,
            "not installed and no longer owned"
        );
        let cached: Vec<u32> = db
            .cached_library()
            .unwrap()
            .iter()
            .map(|r| r.appid)
            .collect();
        assert_eq!(cached, [10, 13]);
    }

    #[test]
    fn clearing_the_cache_keeps_installed_titles_and_their_word_on_drm() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.upsert_app(&installed(10)).unwrap();
        db.cache_library(&[owned(10, "OnDisk"), owned(12, "Other")], "t1")
            .unwrap();
        db.clear_library_cache().unwrap();
        assert_eq!(db.app(12).unwrap(), None);
        assert_eq!(db.cached_library().unwrap().len(), 0);
        let kept = db.app(10).unwrap().unwrap();
        assert_eq!(kept.drm, Drm::None);
        assert_eq!(kept.fetched_at, None);
        assert_eq!(kept.size_bytes, None);
        assert_eq!(kept.last_installed_buildid, Some(5));
        assert_eq!(kept.launch_exe.as_deref(), Some("game.exe"));
    }

    #[test]
    fn set_app_drm_needs_a_row() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert!(!db.set_app_drm(10, Drm::None).unwrap());
        db.upsert_app(&installed(10)).unwrap();
        db.set_app_drm(10, Drm::Unknown).unwrap();
        assert_eq!(db.app(10).unwrap().unwrap().drm, Drm::Unknown);
    }

    #[test]
    fn meta_get_and_set() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert_eq!(db.meta_get("host_chip").unwrap(), None);
        db.meta_set("host_chip", "arm64").unwrap();
        db.meta_set("host_chip", "Apple M4").unwrap();
        assert_eq!(
            db.meta_get("host_chip").unwrap().as_deref(),
            Some("Apple M4")
        );
        assert!(db.meta_get("schema_version").unwrap().is_some());
    }
}
