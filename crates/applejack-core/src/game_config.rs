//! Per-game configuration operations. `set`, `export`, and `import` follow
//! one rule: reads never create `db.sqlite`, and a write that has nothing
//! valid to store leaves the data directory as it found it.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    self, Accepted, AppRow, Confirmation, DefaultEnvPolicy, Drm, Error, Field, GameConfig,
    ImportOptions, Preset, Rejected,
};
use crate::error::CoreError;
use crate::host::{HidrawDecision, HidrawSource};
use crate::runtime::direct::takes_direct_path;
use crate::runtime::steam_client::STEAM_CLIENT_PREFIX;
use crate::state::Core;
use applejack_engine::HidrawMode;

/// Largest preset file `import` reads. A preset is a few hundred bytes of
/// settings; anything bigger is read into memory, written into the database,
/// and echoed back, so the read stops one byte past the cap and the file is
/// refused.
pub const MAX_PRESET_BYTES: u64 = 1 << 20;

/// The one `config set` key that is not a `game_config` column.
const DRM_KEY: &str = "drm";

/// The result of `config.import`. `applied` is false while `confirmations`
/// holds items the request did not accept, and when nothing was acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigImportResult {
    pub applied: bool,
    /// The appid the preset was exported for; the request's appid wins.
    pub preset_appid: u32,
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
    pub confirmations: Vec<Confirmation>,
}

/// The result of `config.resolve`: the settings a run would apply now that
/// the stored row does not state outright. `hidraw_enabled` is the stored
/// value when there is one, else what the connected pads decide (plan 06);
/// `hidraw_reason` is the text a `prepare: hidraw` line carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigResolveResult {
    pub appid: u32,
    pub hidraw_enabled: bool,
    pub hidraw_source: HidrawSource,
    pub hidraw_reason: String,
}

/// The text `set_field` takes for a JSON value: `null` clears, a string is
/// itself, anything else is its JSON (`extra_env` takes an object).
#[must_use]
pub fn config_value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl Core {
    /// The stored row, or the empty configuration.
    ///
    /// # Errors
    ///
    /// Returns the database failure.
    pub fn config_get(&mut self, appid: u32) -> Result<GameConfig, CoreError> {
        self.reading(|db| Ok(db.game_config(appid)?.unwrap_or_default()))
    }

    /// The hidraw mode a run of `appid` would apply now, and why: the stored
    /// row when it sets `hidraw_enabled`, else the connected pads. When the
    /// inventory cannot be read, the prefix the run would use keeps its mode
    /// and a prefix not yet built gets `Disabled`, as the run does.
    ///
    /// # Errors
    ///
    /// Returns the database failure.
    pub fn config_resolve(&mut self, appid: u32) -> Result<ConfigResolveResult, CoreError> {
        let config = self.config_get(appid)?;
        let inventory = self.controllers_for(&config);
        let current = if config.hidraw_enabled.is_none() && !inventory.is_available() {
            self.current_hidraw(appid, &config)?
        } else {
            None
        };
        let decision = HidrawDecision::decide(config.hidraw_enabled, &inventory, current);
        Ok(ConfigResolveResult {
            appid,
            hidraw_enabled: decision.enabled(),
            hidraw_source: decision.source,
            hidraw_reason: decision.reason,
        })
    }

    /// The hidraw mode of the prefix a run of `appid` would use, when that
    /// prefix exists: the client's for a title on the client path, else the
    /// title's own.
    fn current_hidraw(
        &mut self,
        appid: u32,
        config: &GameConfig,
    ) -> Result<Option<HidrawMode>, CoreError> {
        let name = match self.title(appid)? {
            Some(app) if !takes_direct_path(&app, config, false) => STEAM_CLIENT_PREFIX.to_owned(),
            _ => appid.to_string(),
        };
        Ok(self
            .open_prefix(&name)
            .ok()
            .map(|(_, prefix)| prefix.hidraw))
    }

    /// The title's `apps` row, for the fields that live there.
    ///
    /// # Errors
    ///
    /// The database failure.
    pub fn title(&mut self, appid: u32) -> Result<Option<AppRow>, CoreError> {
        self.reading(|db| Ok(db.app(appid)?))
    }

    /// Sets one field from its text form; an empty value clears it.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownField`] or [`Error::InvalidValue`], before the
    /// database is created.
    pub fn config_set(&mut self, appid: u32, key: &str, value: &str) -> Result<(), CoreError> {
        if key == DRM_KEY {
            return self.set_drm(appid, value);
        }
        let field = Field::parse(key).ok_or_else(|| Error::UnknownField(key.to_owned()))?;
        if key == "direct_verified_at" {
            return Err(Error::InvalidValue {
                field: "direct_verified_at",
                value: value.to_owned(),
                reason: "only a direct probe records this".to_owned(),
            }
            .into());
        }
        GameConfig::default().set(field, value)?;
        self.db()?.set_field(appid, key, value)?;
        Ok(())
    }

    /// `drm` is the user's word and lives on the title, not in the per-game
    /// configuration, so a preset cannot carry it. An empty value goes back
    /// to `unknown`.
    fn set_drm(&mut self, appid: u32, value: &str) -> Result<(), CoreError> {
        let drm = if value.is_empty() {
            Drm::Unknown
        } else {
            value.parse().map_err(|reason| Error::InvalidValue {
                field: DRM_KEY,
                value: value.to_owned(),
                reason,
            })?
        };
        let found = self.reading(|db| Ok(db.app(appid)?))?.is_some();
        if !found || !self.db()?.set_app_drm(appid, drm)? {
            return Err(CoreError::TitleNotInstalled {
                appid,
                reason: "it has no record; run `applejack install` first".to_owned(),
            });
        }
        Ok(())
    }

    /// Writes a preset for `appid` to `file`.
    ///
    /// # Errors
    ///
    /// Returns the export or file write failure.
    pub fn config_export(&mut self, appid: u32, file: &Path) -> Result<(), CoreError> {
        let data_dir = self.data_dir().to_path_buf();
        let home = self.home().map(Path::to_path_buf);
        let preset = self.reading(|db| {
            Ok(config::presets::export(
                db,
                appid,
                &data_dir,
                home.as_deref(),
            )?)
        })?;
        let json = preset.to_json_pretty()?;
        std::fs::write(file, format!("{json}\n")).map_err(|error| {
            CoreError::Message(format!("cannot write {}: {error}", file.display()))
        })
    }

    /// Filters a preset file through the engine's environment policy and
    /// applies it unless it needs a confirmation the request did not give.
    /// The request's `appid` wins over the file's.
    ///
    /// # Errors
    ///
    /// Returns the read, parse, or write failure; a preset that needs
    /// confirmation is a result, not an error.
    pub fn config_import(
        &mut self,
        appid: u32,
        file: &Path,
        accept_launch_overrides: bool,
    ) -> Result<ConfigImportResult, CoreError> {
        let mut preset = Preset::from_json(&read_preset(file)?)?;
        let preset_appid = preset.appid;
        preset.appid = appid;
        let data_dir = self.data_dir().to_path_buf();
        let home = self.home().map(Path::to_path_buf);
        let policy = DefaultEnvPolicy::new(&data_dir);
        let options = ImportOptions {
            accept_launch_overrides,
        };
        // The plan only reads; `db.sqlite` is created below, once there is
        // something to write.
        let plan = self.reading(|db| {
            Ok(config::presets::import(
                &preset,
                &policy,
                db,
                &data_dir,
                home.as_deref(),
                options,
            )?)
        })?;
        let applied = !plan.needs_confirmation() && !plan.accepted.is_empty();
        if applied {
            plan.apply(self.db()?)?;
        }
        Ok(ConfigImportResult {
            applied,
            preset_appid,
            accepted: plan.accepted,
            rejected: plan.rejected,
            confirmations: plan.confirmations,
        })
    }
}

fn read_preset(file: &Path) -> Result<String, CoreError> {
    let cannot_read = |error: &dyn std::fmt::Display| {
        CoreError::Message(format!("cannot read {}: {error}", file.display()))
    };
    // Bound the read itself: a FIFO or character device reports no size, and
    // a regular file can grow between a size check and the read. The size is
    // checked before decoding, so a cut inside a multibyte character still
    // reports the size, not a UTF-8 error.
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .map_err(|error| cannot_read(&error))?
        .take(MAX_PRESET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| cannot_read(&error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PRESET_BYTES {
        return Err(cannot_read(&format!(
            "larger than the {MAX_PRESET_BYTES} byte preset limit"
        )));
    }
    String::from_utf8(bytes).map_err(|error| cannot_read(&error))
}
