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
    pub fetched_at: Option<String>,
}

const APP_COLUMNS: &str = "appid, name, installdir, drm, library_dir, last_installed_buildid, last_manifest_ids, fetched_at";

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
        // rusqlite's u64 conversion is behind a feature flag; a build id above
        // i64::MAX cannot be stored in an INTEGER column anyway.
        let buildid = row
            .last_installed_buildid
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::InvalidValue {
                field: "last_installed_buildid",
                value: row.last_installed_buildid.unwrap_or_default().to_string(),
                reason: "exceeds the INTEGER column range".to_owned(),
            })?;
        self.conn.execute(
            &format!(
                "INSERT INTO apps ({APP_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(appid) DO UPDATE SET name = excluded.name, \
                 installdir = excluded.installdir, drm = excluded.drm, \
                 library_dir = excluded.library_dir, \
                 last_installed_buildid = excluded.last_installed_buildid, \
                 last_manifest_ids = excluded.last_manifest_ids, fetched_at = excluded.fetched_at"
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
            ],
        )?;
        Ok(())
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
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
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
            last_installed_buildid: self
                .last_installed_buildid
                .map(u64::try_from)
                .transpose()
                .map_err(|_| Error::InvalidValue {
                    field: "last_installed_buildid",
                    value: self.last_installed_buildid.unwrap_or_default().to_string(),
                    reason: "negative build id".to_owned(),
                })?,
            last_manifest_ids,
            fetched_at: self.fetched_at,
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
