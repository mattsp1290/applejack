//! Layering of engine defaults, an imported preset, and the user's own row
//! into the settings a launch uses.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{GameConfig, GraphicsBackend, LaunchMode, SyncMode};

/// Launch settings with every engine-defaulted field filled in. Fields that
/// have no engine default (engine identity, launch overrides, the DX12
/// expectation) stay optional.
///
/// Values are the stored text as imported or set; nothing here is re-checked
/// against the database or the file system. The launch step validates the
/// overlay for `graphics_backend = d3dmetal` and every path in `extra_env`
/// and the overrides against the engine's roots before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per game_config toggle column; the columns are independent"
)]
pub struct Resolved {
    /// The title these settings are for: the key for its log directory,
    /// session row, and prefix name.
    pub appid: u32,
    pub engine_id: Option<String>,
    pub engine_version: Option<String>,
    pub graphics_backend: GraphicsBackend,
    pub sync_mode: SyncMode,
    pub hidraw_enabled: bool,
    pub steam_input: bool,
    pub steam_overlay: bool,
    pub rosetta_advertise_avx: bool,
    pub extra_env: BTreeMap<String, String>,
    pub launch_mode: LaunchMode,
    pub launch_exe_override: Option<String>,
    pub launch_args_override: Option<String>,
    pub isolated_prefix: bool,
    pub dx12_expectation: Option<String>,
}

/// Picks each field from `user`, else `preset`, else `engine_defaults`.
/// `extra_env` is the union with the same precedence per name. When the
/// engine leaves a defaulted field unset, the conservative value applies:
/// DXMT, no fast sync, every toggle off, the client launch path, a shared
/// prefix.
///
/// The union cannot unset an inherited variable: removing a name from the
/// user layer (`config set extra_env NAME`) only drops the user's own value,
/// and the preset's or engine's value for that name still applies. Unsetting
/// would need a tombstone representation in `extra_env`; none exists yet.
#[must_use]
pub fn resolve(
    appid: u32,
    engine_defaults: &GameConfig,
    preset: Option<&GameConfig>,
    user: &GameConfig,
) -> Resolved {
    let empty = GameConfig::default();
    let preset = preset.unwrap_or(&empty);
    macro_rules! pick {
        ($field:ident) => {
            user.$field
                .clone()
                .or_else(|| preset.$field.clone())
                .or_else(|| engine_defaults.$field.clone())
        };
    }
    let mut extra_env = engine_defaults.extra_env.clone();
    extra_env.extend(preset.extra_env.clone());
    extra_env.extend(user.extra_env.clone());
    Resolved {
        appid,
        engine_id: pick!(engine_id),
        engine_version: pick!(engine_version),
        graphics_backend: pick!(graphics_backend).unwrap_or(GraphicsBackend::Dxmt),
        sync_mode: pick!(sync_mode).unwrap_or(SyncMode::Disabled),
        hidraw_enabled: pick!(hidraw_enabled).unwrap_or(false),
        steam_input: pick!(steam_input).unwrap_or(false),
        steam_overlay: pick!(steam_overlay).unwrap_or(false),
        rosetta_advertise_avx: pick!(rosetta_advertise_avx).unwrap_or(false),
        extra_env,
        launch_mode: pick!(launch_mode).unwrap_or(LaunchMode::Client),
        launch_exe_override: pick!(launch_exe_override),
        launch_args_override: pick!(launch_args_override),
        isolated_prefix: pick!(isolated_prefix).unwrap_or(false),
        dx12_expectation: pick!(dx12_expectation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Field;

    fn layer(
        tag: &str,
        backend: GraphicsBackend,
        sync: SyncMode,
        flag: bool,
        mode: LaunchMode,
    ) -> GameConfig {
        GameConfig {
            engine_id: Some(format!("engine-{tag}")),
            engine_version: Some(format!("version-{tag}")),
            graphics_backend: Some(backend),
            sync_mode: Some(sync),
            hidraw_enabled: Some(flag),
            steam_input: Some(flag),
            steam_overlay: Some(flag),
            rosetta_advertise_avx: Some(flag),
            extra_env: BTreeMap::from([
                ("SHARED".to_owned(), tag.to_owned()),
                (format!("ONLY_{}", tag.to_uppercase()), tag.to_owned()),
            ]),
            launch_mode: Some(mode),
            launch_exe_override: Some(format!("exe-{tag}")),
            launch_args_override: Some(format!("args-{tag}")),
            direct_verified_at: Some(format!("verified-{tag}")),
            isolated_prefix: Some(flag),
            dx12_expectation: Some(format!("dx12-{tag}")),
            notes: Some(format!("notes-{tag}")),
        }
    }

    fn defaults() -> GameConfig {
        layer(
            "default",
            GraphicsBackend::Dxmt,
            SyncMode::Msync,
            false,
            LaunchMode::Client,
        )
    }

    fn preset() -> GameConfig {
        layer(
            "preset",
            GraphicsBackend::Wined3d,
            SyncMode::Esync,
            true,
            LaunchMode::Direct,
        )
    }

    fn user() -> GameConfig {
        layer(
            "user",
            GraphicsBackend::Vkd3dProton,
            SyncMode::Disabled,
            false,
            LaunchMode::Client,
        )
    }

    #[test]
    fn user_beats_preset_beats_default_for_every_field() {
        let resolved = resolve(10, &defaults(), Some(&preset()), &user());
        assert_eq!(resolved.appid, 10);
        assert_eq!(resolved.engine_id.as_deref(), Some("engine-user"));
        assert_eq!(resolved.engine_version.as_deref(), Some("version-user"));
        assert_eq!(resolved.graphics_backend, GraphicsBackend::Vkd3dProton);
        assert_eq!(resolved.sync_mode, SyncMode::Disabled);
        assert!(!resolved.hidraw_enabled);
        assert!(!resolved.steam_input);
        assert!(!resolved.steam_overlay);
        assert!(!resolved.rosetta_advertise_avx);
        assert_eq!(resolved.launch_mode, LaunchMode::Client);
        assert_eq!(resolved.launch_exe_override.as_deref(), Some("exe-user"));
        assert_eq!(resolved.launch_args_override.as_deref(), Some("args-user"));
        assert!(!resolved.isolated_prefix);
        assert_eq!(resolved.dx12_expectation.as_deref(), Some("dx12-user"));
        assert_eq!(resolved.extra_env["SHARED"], "user");
        assert_eq!(resolved.extra_env["ONLY_DEFAULT"], "default");
        assert_eq!(resolved.extra_env["ONLY_PRESET"], "preset");
        assert_eq!(resolved.extra_env["ONLY_USER"], "user");
    }

    #[test]
    fn preset_beats_default_when_user_is_unset() {
        let resolved = resolve(10, &defaults(), Some(&preset()), &GameConfig::default());
        assert_eq!(resolved.engine_id.as_deref(), Some("engine-preset"));
        assert_eq!(resolved.graphics_backend, GraphicsBackend::Wined3d);
        assert_eq!(resolved.sync_mode, SyncMode::Esync);
        assert!(resolved.hidraw_enabled);
        assert!(resolved.steam_input);
        assert!(resolved.steam_overlay);
        assert!(resolved.rosetta_advertise_avx);
        assert_eq!(resolved.launch_mode, LaunchMode::Direct);
        assert_eq!(resolved.launch_exe_override.as_deref(), Some("exe-preset"));
        assert!(resolved.isolated_prefix);
        assert_eq!(resolved.extra_env["SHARED"], "preset");
    }

    #[test]
    fn default_applies_when_preset_and_user_are_unset() {
        let resolved = resolve(10, &defaults(), None, &GameConfig::default());
        assert_eq!(resolved.engine_id.as_deref(), Some("engine-default"));
        assert_eq!(resolved.graphics_backend, GraphicsBackend::Dxmt);
        assert_eq!(resolved.sync_mode, SyncMode::Msync);
        assert_eq!(resolved.launch_mode, LaunchMode::Client);
        assert_eq!(resolved.extra_env["SHARED"], "default");
        assert_eq!(resolved.extra_env.len(), 2);
    }

    #[test]
    fn each_field_set_on_one_layer_only_wins_from_that_layer() {
        for field in Field::ALL {
            if matches!(
                field,
                Field::ExtraEnv | Field::Notes | Field::DirectVerifiedAt
            ) {
                continue;
            }
            let mut user_only = GameConfig::default();
            user_only.set(field, &user().get(field).unwrap()).unwrap();
            let resolved = resolve(10, &defaults(), Some(&preset()), &user_only);
            let json = serde_json::to_value(&resolved).unwrap();
            let expected = serde_json::to_value(&user_only).unwrap()[field.name()].clone();
            assert_eq!(json[field.name()], expected, "{field}");
        }
    }

    #[test]
    fn empty_engine_defaults_fall_back_to_conservative_values() {
        let resolved = resolve(10, &GameConfig::default(), None, &GameConfig::default());
        assert_eq!(resolved.graphics_backend, GraphicsBackend::Dxmt);
        assert_eq!(resolved.sync_mode, SyncMode::Disabled);
        assert_eq!(resolved.launch_mode, LaunchMode::Client);
        assert!(!resolved.hidraw_enabled && !resolved.isolated_prefix);
        assert!(resolved.extra_env.is_empty());
    }
}
