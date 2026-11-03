//! The client-free launch path: which titles take it, and what it runs.

use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use applejack_engine::rosetta_wine::launch::pe_machine;

use crate::config::{AppRow, Drm, GameConfig, LaunchMode};
use crate::error::CoreError;

/// Whether a run goes straight to the title's exe. `probe_direct` always
/// does: that is what it is for. Otherwise `drm = none` does, unless the
/// user asked for the client, and `launch_mode = direct` does only with
/// `direct_verified_at` behind it.
#[must_use]
pub fn takes_direct_path(app: &AppRow, config: &GameConfig, probe_direct: bool) -> bool {
    if probe_direct {
        return true;
    }
    match config.launch_mode {
        Some(LaunchMode::Client) => false,
        Some(LaunchMode::Direct) => app.drm == Drm::None || config.direct_verified_at.is_some(),
        None => app.drm == Drm::None,
    }
}

/// What a direct launch hands the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectLaunch {
    /// Absolute Unix path; Wine reaches it through `Z:`.
    pub exe: PathBuf,
    /// The exe's file name: the game-running marker.
    pub exe_name: String,
    /// Whitespace-split `launch_args_override`.
    pub args: Vec<OsString>,
    /// The exe's directory, which is where titles look for their data.
    pub cwd: PathBuf,
}

/// Resolves `launch_exe_override` against the install directory. The value
/// may come from a preset, so it stays inside that directory, symlinks
/// followed, and must be a PE image: Wine's answer to anything else looks
/// like a title that needs the client.
///
/// # Errors
///
/// [`CoreError::NoLaunchExe`] with the reason.
pub fn resolve_launch(
    appid: u32,
    install_dir: &Path,
    config: &GameConfig,
) -> Result<DirectLaunch, CoreError> {
    let refuse = |reason: String| CoreError::NoLaunchExe {
        appid,
        reason: format!(
            "{reason}; set one with `applejack config {appid} set launch_exe_override <file>`"
        ),
    };
    let relative = config
        .launch_exe_override
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| refuse("none is recorded".to_owned()))?;
    let relative = Path::new(relative);
    let plain = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !plain {
        return Err(refuse(format!(
            "`{}` is not a path inside the install directory",
            relative.display()
        )));
    }
    let canonical = |path: &Path| {
        fs::canonicalize(path).map_err(|error| refuse(format!("{}: {error}", path.display())))
    };
    let root = canonical(install_dir)?;
    let exe = canonical(&install_dir.join(relative))?;
    if !exe.starts_with(&root) || !exe.is_file() {
        return Err(refuse(format!(
            "`{}` is not a file inside the install directory",
            relative.display()
        )));
    }
    let image = pe_machine(&exe).map_err(|error| refuse(format!("{}: {error}", exe.display())))?;
    if image.is_none() {
        return Err(refuse(format!(
            "`{}` is not a Windows executable",
            relative.display()
        )));
    }
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let exe_name = name(&exe).unwrap_or_default();
    let cwd = exe.parent().unwrap_or(&root).to_path_buf();
    let args = config
        .launch_args_override
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .map(OsString::from)
        .collect();
    Ok(DirectLaunch {
        exe,
        exe_name,
        args,
        cwd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The least [`pe_machine`] reads as an image.
    fn pe() -> Vec<u8> {
        let mut image = vec![0; 64];
        image[..2].copy_from_slice(b"MZ");
        image[60] = 64;
        image.extend_from_slice(b"PE\0\0\x64\x86");
        image
    }

    fn app(drm: Drm) -> AppRow {
        AppRow {
            appid: 10,
            drm,
            ..AppRow::default()
        }
    }

    fn config(mode: Option<LaunchMode>, verified: bool) -> GameConfig {
        GameConfig {
            launch_mode: mode,
            direct_verified_at: verified.then(|| "2026-11-01T00:00:00Z".to_owned()),
            ..GameConfig::default()
        }
    }

    #[test]
    fn only_a_client_free_title_or_a_probe_takes_the_direct_path() {
        let plain = config(None, false);
        assert!(takes_direct_path(&app(Drm::None), &plain, false));
        assert!(!takes_direct_path(&app(Drm::Unknown), &plain, false));
        assert!(!takes_direct_path(&app(Drm::Steam), &plain, false));
        assert!(takes_direct_path(&app(Drm::Unknown), &plain, true));
        assert!(takes_direct_path(&app(Drm::Steam), &plain, true));
    }

    #[test]
    fn launch_mode_direct_needs_the_verification_and_client_wins_over_drm_none() {
        let unverified = config(Some(LaunchMode::Direct), false);
        let verified = config(Some(LaunchMode::Direct), true);
        assert!(!takes_direct_path(&app(Drm::Unknown), &unverified, false));
        assert!(takes_direct_path(&app(Drm::Unknown), &verified, false));
        let client = config(Some(LaunchMode::Client), false);
        assert!(!takes_direct_path(&app(Drm::None), &client, false));
    }

    fn with_exe(value: &str) -> GameConfig {
        GameConfig {
            launch_exe_override: Some(value.to_owned()),
            launch_args_override: Some("-a  -b".to_owned()),
            ..GameConfig::default()
        }
    }

    #[test]
    fn the_exe_resolves_inside_the_install_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        fs::write(dir.path().join("bin/game.exe"), pe()).unwrap();
        let launch = resolve_launch(10, dir.path(), &with_exe("bin/game.exe")).unwrap();
        assert_eq!(launch.exe_name, "game.exe");
        assert_eq!(launch.cwd.file_name().unwrap(), "bin");
        assert_eq!(launch.args, ["-a", "-b"]);
    }

    #[test]
    fn a_file_that_is_not_a_pe_image_is_refused() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("text.exe"), b"MZ but no more").unwrap();
        let error = resolve_launch(10, dir.path(), &with_exe("text.exe")).unwrap_err();
        assert!(matches!(error, CoreError::NoLaunchExe { .. }), "{error}");
        assert!(error.to_string().contains("not a Windows executable"));
        assert!(error.to_string().contains("set launch_exe_override"));
    }

    #[test]
    fn an_exe_that_leaves_the_install_dir_is_refused() {
        let root = TempDir::new().unwrap();
        let install = root.path().join("title");
        fs::create_dir(&install).unwrap();
        fs::write(root.path().join("outside.exe"), pe()).unwrap();
        std::os::unix::fs::symlink(root.path().join("outside.exe"), install.join("link.exe"))
            .unwrap();
        let absolute = root.path().join("outside.exe");
        for value in [
            "../outside.exe",
            "link.exe",
            "missing.exe",
            absolute.to_str().unwrap(),
        ] {
            let error = resolve_launch(10, &install, &with_exe(value)).unwrap_err();
            assert!(matches!(error, CoreError::NoLaunchExe { .. }), "{value}");
            assert!(
                error.to_string().contains("launch_exe_override <file>"),
                "{error}"
            );
        }
        let error = resolve_launch(10, &install, &GameConfig::default()).unwrap_err();
        assert!(
            error.to_string().contains("set launch_exe_override"),
            "{error}"
        );
    }
}
