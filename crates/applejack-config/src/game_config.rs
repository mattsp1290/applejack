//! Typed view of the `game_config` table and the table-driven field parser
//! behind `config set`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{ConfigDb, Error};

/// Graphics translation layer a title runs on. The engine crate has its own
/// enum; `applejack-core` maps between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsBackend {
    /// DXMT for DirectX 10 and 11.
    #[serde(rename = "dxmt")]
    Dxmt,
    /// Wine's own Direct3D over OpenGL, the DirectX 9 default.
    #[serde(rename = "wined3d")]
    Wined3d,
    /// `vkd3d-proton` over a Vulkan-on-Metal driver for DirectX 12.
    #[serde(rename = "vkd3d-proton")]
    Vkd3dProton,
    /// Apple's `D3DMetal` from a user-supplied GPTK overlay.
    #[serde(rename = "d3dmetal")]
    D3dmetal,
}

impl GraphicsBackend {
    /// Every backend in declaration order.
    pub const ALL: [Self; 4] = [Self::Dxmt, Self::Wined3d, Self::Vkd3dProton, Self::D3dmetal];

    /// The name used in the database, presets, and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dxmt => "dxmt",
            Self::Wined3d => "wined3d",
            Self::Vkd3dProton => "vkd3d-proton",
            Self::D3dmetal => "d3dmetal",
        }
    }
}

/// Wine synchronization primitive, per decision 7 of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMode {
    /// Mach semaphores plus `ulock` (out-of-tree patch set).
    #[serde(rename = "msync")]
    Msync,
    /// eventfd-style synchronization.
    #[serde(rename = "esync")]
    Esync,
    /// No fast synchronization; wineserver round trips.
    #[serde(rename = "none")]
    Disabled,
}

impl SyncMode {
    /// Every mode in declaration order.
    pub const ALL: [Self; 3] = [Self::Msync, Self::Esync, Self::Disabled];

    /// The name used in the database, presets, and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Msync => "msync",
            Self::Esync => "esync",
            Self::Disabled => "none",
        }
    }
}

/// How a title is started: through the Windows Steam client in the shared
/// prefix, or directly without a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchMode {
    /// `steam.exe -applaunch <appid>` inside the client prefix.
    #[serde(rename = "client")]
    Client,
    /// Run the title's executable without a Steam client.
    #[serde(rename = "direct")]
    Direct,
}

impl LaunchMode {
    /// Every mode in declaration order.
    pub const ALL: [Self; 2] = [Self::Client, Self::Direct];

    /// The name used in the database (a CHECK constraint), presets, and the CLI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Direct => "direct",
        }
    }
}

macro_rules! named_enum {
    ($ty:ident) => {
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $ty {
            type Err = String;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .into_iter()
                    .find(|candidate| candidate.as_str() == text)
                    .ok_or_else(|| {
                        format!(
                            "expected one of {}",
                            Self::ALL
                                .iter()
                                .map(|candidate| candidate.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
            }
        }
    };
}

named_enum!(GraphicsBackend);
named_enum!(SyncMode);
named_enum!(LaunchMode);

/// One `game_config` row. Every column is optional because a row only
/// records what the user or a preset set; `resolve` fills the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameConfig {
    pub engine_id: Option<String>,
    pub engine_version: Option<String>,
    pub graphics_backend: Option<GraphicsBackend>,
    pub sync_mode: Option<SyncMode>,
    pub hidraw_enabled: Option<bool>,
    pub steam_input: Option<bool>,
    pub steam_overlay: Option<bool>,
    pub rosetta_advertise_avx: Option<bool>,
    pub extra_env: BTreeMap<String, String>,
    pub launch_mode: Option<LaunchMode>,
    pub launch_exe_override: Option<String>,
    pub launch_args_override: Option<String>,
    pub direct_verified_at: Option<String>,
    pub isolated_prefix: Option<bool>,
    pub dx12_expectation: Option<String>,
    pub notes: Option<String>,
}

/// A `game_config` column addressable by name from `config set`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    EngineId,
    EngineVersion,
    GraphicsBackend,
    SyncMode,
    HidrawEnabled,
    SteamInput,
    SteamOverlay,
    RosettaAdvertiseAvx,
    ExtraEnv,
    LaunchMode,
    LaunchExeOverride,
    LaunchArgsOverride,
    DirectVerifiedAt,
    IsolatedPrefix,
    Dx12Expectation,
    Notes,
}

/// One row of the field table: the column name, how to parse a CLI value
/// into it, and how to read it back as text.
struct FieldSpec {
    name: &'static str,
    field: Field,
    set: fn(&mut GameConfig, &str) -> Result<(), String>,
    get: fn(&GameConfig) -> Option<String>,
}

/// The field table. `config set` can only write a column that has a row here,
/// and the SQL column list is derived from it so the two cannot drift.
const FIELDS: [FieldSpec; 16] = [
    FieldSpec {
        name: "engine_id",
        field: Field::EngineId,
        set: |config, value| {
            config.engine_id = identifier(value)?;
            Ok(())
        },
        get: |config| config.engine_id.clone(),
    },
    FieldSpec {
        name: "engine_version",
        field: Field::EngineVersion,
        set: |config, value| {
            config.engine_version = identifier(value)?;
            Ok(())
        },
        get: |config| config.engine_version.clone(),
    },
    FieldSpec {
        name: "graphics_backend",
        field: Field::GraphicsBackend,
        set: |config, value| {
            config.graphics_backend = parse_enum(value)?;
            Ok(())
        },
        get: |config| config.graphics_backend.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "sync_mode",
        field: Field::SyncMode,
        set: |config, value| {
            config.sync_mode = parse_enum(value)?;
            Ok(())
        },
        get: |config| config.sync_mode.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "hidraw_enabled",
        field: Field::HidrawEnabled,
        set: |config, value| {
            config.hidraw_enabled = parse_bool(value)?;
            Ok(())
        },
        get: |config| config.hidraw_enabled.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "steam_input",
        field: Field::SteamInput,
        set: |config, value| {
            config.steam_input = parse_bool(value)?;
            Ok(())
        },
        get: |config| config.steam_input.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "steam_overlay",
        field: Field::SteamOverlay,
        set: |config, value| {
            config.steam_overlay = parse_bool(value)?;
            Ok(())
        },
        get: |config| config.steam_overlay.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "rosetta_advertise_avx",
        field: Field::RosettaAdvertiseAvx,
        set: |config, value| {
            config.rosetta_advertise_avx = parse_bool(value)?;
            Ok(())
        },
        get: |config| config.rosetta_advertise_avx.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "extra_env",
        field: Field::ExtraEnv,
        set: set_extra_env,
        // JSON, so `get` output feeds `set` back unchanged.
        get: |config| {
            (!config.extra_env.is_empty())
                .then(|| serde_json::to_string(&config.extra_env).ok())
                .flatten()
        },
    },
    FieldSpec {
        name: "launch_mode",
        field: Field::LaunchMode,
        set: |config, value| {
            config.launch_mode = parse_enum(value)?;
            Ok(())
        },
        get: |config| config.launch_mode.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "launch_exe_override",
        field: Field::LaunchExeOverride,
        set: |config, value| {
            config.launch_exe_override = text(value);
            Ok(())
        },
        get: |config| config.launch_exe_override.clone(),
    },
    FieldSpec {
        name: "launch_args_override",
        field: Field::LaunchArgsOverride,
        set: |config, value| {
            config.launch_args_override = text(value);
            Ok(())
        },
        get: |config| config.launch_args_override.clone(),
    },
    FieldSpec {
        name: "direct_verified_at",
        field: Field::DirectVerifiedAt,
        set: |config, value| {
            config.direct_verified_at = text(value);
            Ok(())
        },
        get: |config| config.direct_verified_at.clone(),
    },
    FieldSpec {
        name: "isolated_prefix",
        field: Field::IsolatedPrefix,
        set: |config, value| {
            config.isolated_prefix = parse_bool(value)?;
            Ok(())
        },
        get: |config| config.isolated_prefix.map(|v| v.to_string()),
    },
    FieldSpec {
        name: "dx12_expectation",
        field: Field::Dx12Expectation,
        set: |config, value| {
            config.dx12_expectation = text(value);
            Ok(())
        },
        get: |config| config.dx12_expectation.clone(),
    },
    FieldSpec {
        name: "notes",
        field: Field::Notes,
        set: |config, value| {
            config.notes = text(value);
            Ok(())
        },
        get: |config| config.notes.clone(),
    },
];

impl Field {
    /// Every field in column order.
    pub const ALL: [Self; 16] = {
        let mut all = [Self::EngineId; 16];
        let mut i = 0;
        while i < FIELDS.len() {
            all[i] = FIELDS[i].field;
            i += 1;
        }
        all
    };

    /// Looks a field up by its column name.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        FIELDS
            .iter()
            .find(|spec| spec.name == key)
            .map(|spec| spec.field)
    }

    /// The column name.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.spec().name
    }

    /// Every column name in column order.
    #[must_use]
    pub fn names() -> Vec<&'static str> {
        FIELDS.iter().map(|spec| spec.name).collect()
    }

    fn spec(self) -> &'static FieldSpec {
        FIELDS
            .iter()
            .find(|spec| spec.field == self)
            .expect("every Field variant has a table row")
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl GameConfig {
    /// Parses `value` for `field` and stores it. An empty value clears the
    /// field. `extra_env` accepts a JSON object (replaces the map),
    /// `NAME=VALUE` (sets one entry), or a bare `NAME` (removes one entry).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidValue`] when `value` does not parse.
    pub fn set(&mut self, field: Field, value: &str) -> Result<(), Error> {
        let spec = field.spec();
        (spec.set)(self, value).map_err(|reason| Error::InvalidValue {
            field: spec.name,
            value: value.to_owned(),
            reason,
        })
    }

    /// The field's value as the text `set` would accept, or `None` when unset.
    #[must_use]
    pub fn get(&self, field: Field) -> Option<String> {
        (field.spec().get)(self)
    }

    /// Overlays every set field of `other` onto `self`; `extra_env` entries
    /// are merged with `other` winning on a shared name.
    pub fn merge_from(&mut self, other: &Self) {
        macro_rules! overlay {
            ($($field:ident),*) => {
                $(if other.$field.is_some() {
                    self.$field.clone_from(&other.$field);
                })*
            };
        }
        overlay!(
            engine_id,
            engine_version,
            graphics_backend,
            sync_mode,
            hidraw_enabled,
            steam_input,
            steam_overlay,
            rosetta_advertise_avx,
            launch_mode,
            launch_exe_override,
            launch_args_override,
            direct_verified_at,
            isolated_prefix,
            dx12_expectation,
            notes
        );
        for (name, value) in &other.extra_env {
            self.extra_env.insert(name.clone(), value.clone());
        }
    }
}

/// Engine ids and versions become directory names under `engines/`, so they
/// are limited to a charset that cannot traverse or hide.
fn identifier(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    let safe = !value.starts_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if safe {
        Ok(Some(value.to_owned()))
    } else {
        Err("expected letters, digits, '.', '_' or '-', not starting with '.'".to_owned())
    }
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_enum<T: FromStr<Err = String>>(value: &str) -> Result<Option<T>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some)
}

fn parse_bool(value: &str) -> Result<Option<bool>, String> {
    match value {
        "" => Ok(None),
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err("expected true or false".to_owned()),
    }
}

fn set_extra_env(config: &mut GameConfig, value: &str) -> Result<(), String> {
    if value.is_empty() {
        config.extra_env.clear();
        return Ok(());
    }
    if value.starts_with('{') {
        let map: BTreeMap<String, String> = serde_json::from_str(value)
            .map_err(|error| format!("not a JSON object of strings: {error}"))?;
        for name in map.keys() {
            check_env_name(name)?;
        }
        config.extra_env = map;
        return Ok(());
    }
    if let Some((name, entry)) = value.split_once('=') {
        check_env_name(name)?;
        config.extra_env.insert(name.to_owned(), entry.to_owned());
    } else {
        check_env_name(value)?;
        config.extra_env.remove(value);
    }
    Ok(())
}

/// Accepts the POSIX portable environment-name alphabet only, so a value can
/// never smuggle a second variable or shell syntax through the name.
pub(crate) fn check_env_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if valid_start && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(format!("{name:?} is not a valid environment variable name"))
    }
}

const UPSERT_SQL: &str = "INSERT INTO game_config (appid, \
     engine_id, engine_version, graphics_backend, sync_mode, hidraw_enabled, steam_input, \
     steam_overlay, rosetta_advertise_avx, extra_env, launch_mode, launch_exe_override, \
     launch_args_override, direct_verified_at, isolated_prefix, dx12_expectation, notes) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
     ON CONFLICT(appid) DO UPDATE SET \
     engine_id = excluded.engine_id, engine_version = excluded.engine_version, \
     graphics_backend = excluded.graphics_backend, sync_mode = excluded.sync_mode, \
     hidraw_enabled = excluded.hidraw_enabled, steam_input = excluded.steam_input, \
     steam_overlay = excluded.steam_overlay, rosetta_advertise_avx = excluded.rosetta_advertise_avx, \
     extra_env = excluded.extra_env, launch_mode = excluded.launch_mode, \
     launch_exe_override = excluded.launch_exe_override, \
     launch_args_override = excluded.launch_args_override, \
     direct_verified_at = excluded.direct_verified_at, isolated_prefix = excluded.isolated_prefix, \
     dx12_expectation = excluded.dx12_expectation, notes = excluded.notes";

const SELECT_SQL: &str = "SELECT \
     engine_id, engine_version, graphics_backend, sync_mode, hidraw_enabled, steam_input, \
     steam_overlay, rosetta_advertise_avx, extra_env, launch_mode, launch_exe_override, \
     launch_args_override, direct_verified_at, isolated_prefix, dx12_expectation, notes \
     FROM game_config WHERE appid = ?1";

impl ConfigDb {
    /// The stored configuration for `appid`, or `None` when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the query fails, [`Error::Json`] when
    /// `extra_env` is not a JSON object, and [`Error::InvalidValue`] when an
    /// enum column holds an unknown name.
    pub fn game_config(&self, appid: u32) -> Result<Option<GameConfig>, Error> {
        let row: Option<RawRow> = self
            .conn
            .query_row(SELECT_SQL, [appid], |row| {
                Ok(RawRow {
                    engine_id: row.get(0)?,
                    engine_version: row.get(1)?,
                    graphics_backend: row.get(2)?,
                    sync_mode: row.get(3)?,
                    hidraw_enabled: row.get(4)?,
                    steam_input: row.get(5)?,
                    steam_overlay: row.get(6)?,
                    rosetta_advertise_avx: row.get(7)?,
                    extra_env: row.get(8)?,
                    launch_mode: row.get(9)?,
                    launch_exe_override: row.get(10)?,
                    launch_args_override: row.get(11)?,
                    direct_verified_at: row.get(12)?,
                    isolated_prefix: row.get(13)?,
                    dx12_expectation: row.get(14)?,
                    notes: row.get(15)?,
                })
            })
            .optional()?;
        row.map(RawRow::into_config).transpose()
    }

    /// Writes the whole row for `appid`, replacing any existing one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Sqlite`] when the write fails or [`Error::Json`] when
    /// `extra_env` cannot be encoded.
    pub fn set_game_config(&self, appid: u32, config: &GameConfig) -> Result<(), Error> {
        let extra_env = serde_json::to_string(&config.extra_env)?;
        self.conn.execute(
            UPSERT_SQL,
            params![
                appid,
                config.engine_id,
                config.engine_version,
                config.graphics_backend.map(GraphicsBackend::as_str),
                config.sync_mode.map(SyncMode::as_str),
                config.hidraw_enabled,
                config.steam_input,
                config.steam_overlay,
                config.rosetta_advertise_avx,
                extra_env,
                config.launch_mode.map(LaunchMode::as_str),
                config.launch_exe_override,
                config.launch_args_override,
                config.direct_verified_at,
                config.isolated_prefix,
                config.dx12_expectation,
                config.notes,
            ],
        )?;
        Ok(())
    }

    /// Parses `value` for the column named `key` and writes it, creating the
    /// row when needed. Unknown keys and unparseable values write nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownField`] for a key that is not a column,
    /// [`Error::InvalidValue`] when the value does not parse, and the read or
    /// write errors of [`ConfigDb::game_config`] and
    /// [`ConfigDb::set_game_config`].
    pub fn set_field(&self, appid: u32, key: &str, value: &str) -> Result<(), Error> {
        let field = Field::parse(key).ok_or_else(|| Error::UnknownField(key.to_owned()))?;
        let mut config = self.game_config(appid)?.unwrap_or_default();
        config.set(field, value)?;
        self.set_game_config(appid, &config)
    }
}

struct RawRow {
    engine_id: Option<String>,
    engine_version: Option<String>,
    graphics_backend: Option<String>,
    sync_mode: Option<String>,
    hidraw_enabled: Option<bool>,
    steam_input: Option<bool>,
    steam_overlay: Option<bool>,
    rosetta_advertise_avx: Option<bool>,
    extra_env: Option<String>,
    launch_mode: Option<String>,
    launch_exe_override: Option<String>,
    launch_args_override: Option<String>,
    direct_verified_at: Option<String>,
    isolated_prefix: Option<bool>,
    dx12_expectation: Option<String>,
    notes: Option<String>,
}

impl RawRow {
    fn into_config(self) -> Result<GameConfig, Error> {
        let extra_env = match self.extra_env.as_deref().map(str::trim) {
            None | Some("") => BTreeMap::new(),
            Some(json) => serde_json::from_str(json)?,
        };
        Ok(GameConfig {
            engine_id: self.engine_id,
            engine_version: self.engine_version,
            graphics_backend: stored_enum("graphics_backend", self.graphics_backend)?,
            sync_mode: stored_enum("sync_mode", self.sync_mode)?,
            hidraw_enabled: self.hidraw_enabled,
            steam_input: self.steam_input,
            steam_overlay: self.steam_overlay,
            rosetta_advertise_avx: self.rosetta_advertise_avx,
            extra_env,
            launch_mode: stored_enum("launch_mode", self.launch_mode)?,
            launch_exe_override: self.launch_exe_override,
            launch_args_override: self.launch_args_override,
            direct_verified_at: self.direct_verified_at,
            isolated_prefix: self.isolated_prefix,
            dx12_expectation: self.dx12_expectation,
            notes: self.notes,
        })
    }
}

fn stored_enum<T: FromStr<Err = String>>(
    field: &'static str,
    value: Option<String>,
) -> Result<Option<T>, Error> {
    value
        .map(|text| {
            text.parse().map_err(|reason| Error::InvalidValue {
                field,
                value: text,
                reason,
            })
        })
        .transpose()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn full_config() -> GameConfig {
        GameConfig {
            engine_id: Some("rosetta-wine".to_owned()),
            engine_version: Some("1.2.3".to_owned()),
            graphics_backend: Some(GraphicsBackend::Vkd3dProton),
            sync_mode: Some(SyncMode::Esync),
            hidraw_enabled: Some(true),
            steam_input: Some(false),
            steam_overlay: Some(true),
            rosetta_advertise_avx: Some(false),
            extra_env: BTreeMap::from([
                ("WINEDEBUG".to_owned(), "-all".to_owned()),
                ("DXMT_LOG_LEVEL".to_owned(), "info".to_owned()),
            ]),
            launch_mode: Some(LaunchMode::Direct),
            launch_exe_override: Some("game/bin/game.exe".to_owned()),
            launch_args_override: Some("-windowed".to_owned()),
            direct_verified_at: Some("2026-10-28T00:00:00Z".to_owned()),
            isolated_prefix: Some(true),
            dx12_expectation: Some("vkd3d".to_owned()),
            notes: Some("works".to_owned()),
        }
    }

    #[test]
    fn every_field_round_trips_through_the_database() {
        let db = ConfigDb::open_in_memory().unwrap();
        assert_eq!(db.game_config(10).unwrap(), None);
        let config = full_config();
        for field in Field::ALL {
            assert!(config.get(field).is_some(), "{field} unset in fixture");
        }
        db.set_game_config(10, &config).unwrap();
        assert_eq!(db.game_config(10).unwrap(), Some(config.clone()));
        let cleared = GameConfig::default();
        db.set_game_config(10, &cleared).unwrap();
        assert_eq!(db.game_config(10).unwrap(), Some(cleared));
    }

    #[test]
    fn every_field_round_trips_through_set_and_get() {
        let full = full_config();
        let mut rebuilt = GameConfig::default();
        for field in Field::ALL {
            let value = full.get(field).unwrap();
            rebuilt.set(field, &value).unwrap();
        }
        assert_eq!(rebuilt, full);
        for field in Field::ALL {
            rebuilt.set(field, "").unwrap();
        }
        assert_eq!(rebuilt, GameConfig::default());
    }

    #[test]
    fn set_field_rejects_unknown_key_and_writes_nothing() {
        let db = ConfigDb::open_in_memory().unwrap();
        let error = db.set_field(10, "colour", "blue").unwrap_err();
        assert!(
            matches!(error, Error::UnknownField(ref key) if key == "colour"),
            "{error}"
        );
        assert!(error.to_string().contains("hidraw_enabled"), "{error}");
        assert_eq!(db.game_config(10).unwrap(), None);
    }

    #[test]
    fn set_field_rejects_bad_typed_values_and_writes_nothing() {
        let db = ConfigDb::open_in_memory().unwrap();
        for (key, value) in [
            ("hidraw_enabled", "maybe"),
            ("engine_id", "../../tmp/x"),
            ("engine_version", ".hidden"),
            ("engine_version", "1.0/evil"),
            ("graphics_backend", "dxvk"),
            ("sync_mode", "ntsync"),
            ("launch_mode", "shell"),
            ("extra_env", "1BAD=x"),
            ("extra_env", "{\"A\": 1}"),
        ] {
            let error = db.set_field(10, key, value).unwrap_err();
            assert!(
                matches!(error, Error::InvalidValue { field, .. } if field == key),
                "{key}={value}: {error}"
            );
        }
        assert_eq!(db.game_config(10).unwrap(), None);
    }

    #[test]
    fn set_field_creates_then_updates_one_column() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.set_field(10, "hidraw_enabled", "true").unwrap();
        db.set_field(10, "extra_env", "WINEDEBUG=-all").unwrap();
        db.set_field(10, "extra_env", "DXMT_LOG_LEVEL=info")
            .unwrap();
        let config = db.game_config(10).unwrap().unwrap();
        assert_eq!(config.hidraw_enabled, Some(true));
        assert_eq!(config.extra_env.len(), 2);
        db.set_field(10, "extra_env", "WINEDEBUG").unwrap();
        db.set_field(10, "hidraw_enabled", "").unwrap();
        let config = db.game_config(10).unwrap().unwrap();
        assert_eq!(config.hidraw_enabled, None);
        assert_eq!(
            config.extra_env.keys().collect::<Vec<_>>(),
            ["DXMT_LOG_LEVEL"]
        );
        db.set_field(10, "extra_env", "{\"A\":\"1\"}").unwrap();
        assert_eq!(
            db.game_config(10).unwrap().unwrap().extra_env,
            BTreeMap::from([("A".to_owned(), "1".to_owned())])
        );
    }

    #[test]
    fn enum_names_match_serde_and_display() {
        for backend in GraphicsBackend::ALL {
            let json = serde_json::to_string(&backend).unwrap();
            assert_eq!(json, format!("\"{backend}\""));
            assert_eq!(
                backend.as_str().parse::<GraphicsBackend>().unwrap(),
                backend
            );
        }
        for mode in SyncMode::ALL {
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{mode}\""));
        }
        for mode in LaunchMode::ALL {
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{mode}\""));
        }
        assert_eq!(GraphicsBackend::Vkd3dProton.as_str(), "vkd3d-proton");
        assert_eq!(SyncMode::Disabled.as_str(), "none");
    }

    #[test]
    fn stored_unknown_enum_is_reported_not_panicked() {
        let db = ConfigDb::open_in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO game_config (appid, graphics_backend) VALUES (10, 'dxvk')",
                [],
            )
            .unwrap();
        let error = db.game_config(10).unwrap_err();
        assert!(
            matches!(
                error,
                Error::InvalidValue {
                    field: "graphics_backend",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn merge_overlays_set_fields_only() {
        let mut base = full_config();
        let overlay = GameConfig {
            hidraw_enabled: Some(false),
            extra_env: BTreeMap::from([
                ("WINEDEBUG".to_owned(), "+all".to_owned()),
                ("NEW".to_owned(), "1".to_owned()),
            ]),
            ..GameConfig::default()
        };
        base.merge_from(&overlay);
        assert_eq!(base.hidraw_enabled, Some(false));
        assert_eq!(base.engine_id.as_deref(), Some("rosetta-wine"));
        assert_eq!(base.extra_env["WINEDEBUG"], "+all");
        assert_eq!(base.extra_env["DXMT_LOG_LEVEL"], "info");
        assert_eq!(base.extra_env["NEW"], "1");
    }
}
