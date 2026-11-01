//! Per-game configuration operations. `set`, `export`, and `import` follow
//! one rule: reads never create `db.sqlite`, and a write that has nothing
//! valid to store leaves the data directory as it found it.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    self, Accepted, Confirmation, DefaultEnvPolicy, Error, Field, GameConfig, ImportOptions,
    Preset, Rejected,
};
use crate::error::CoreError;
use crate::state::Core;

/// Largest preset file `import` reads. A preset is a few hundred bytes of
/// settings; anything bigger is read into memory, written into the database,
/// and echoed back, so the read stops one byte past the cap and the file is
/// refused.
pub const MAX_PRESET_BYTES: u64 = 1 << 20;

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

    /// Sets one field from its text form; an empty value clears it.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownField`] or [`Error::InvalidValue`], before the
    /// database is created.
    pub fn config_set(&mut self, appid: u32, key: &str, value: &str) -> Result<(), CoreError> {
        let field = Field::parse(key).ok_or_else(|| Error::UnknownField(key.to_owned()))?;
        GameConfig::default().set(field, value)?;
        self.db()?.set_field(appid, key, value)?;
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
