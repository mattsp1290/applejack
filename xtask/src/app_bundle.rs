//! `cargo xtask app-bundle`: assembles `build/Applejack.app` from the `SwiftPM`
//! product, the `applejack` binary as `applejack-core`, and the Info.plist.
//! The tree is built under `<app>.tmp` and swapped into place, as
//! `build/assemble.sh` does for the engine payload, so a failed run leaves
//! the previous bundle untouched. No signing: work package 6d adds it.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "usage: cargo xtask app-bundle [--debug]";
const SWIFT_PACKAGE: &str = "apps/Applejack";
const SHELL_PRODUCT: &str = "ApplejackApp";
const INFO_PLIST: &str = "apps/Applejack/Resources/Info.plist";
/// `CFBundlePackageType` and `CFBundleSignature` as Finder reads them.
const PKG_INFO: &[u8] = b"APPL????";

/// Which build of the shell and the core go into the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Debug,
    Release,
}

impl Profile {
    const fn swift_configuration(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }

    /// `build/Applejack.app` for release; the debug bundle `check` assembles
    /// keeps its own name so it never replaces a release build.
    const fn app_name(self) -> &'static str {
        match self {
            Self::Debug => "Applejack-debug.app",
            Self::Release => "Applejack.app",
        }
    }
}

/// Every path inside the bundle. `Contents/Resources` is created empty: the
/// shell has no asset catalog yet, and the entitlements file is a signing
/// input (6d), not a bundle member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppBundleLayout {
    pub app: PathBuf,
}

impl AppBundleLayout {
    pub fn new(app: impl Into<PathBuf>) -> Self {
        Self { app: app.into() }
    }

    pub fn contents(&self) -> PathBuf {
        self.app.join("Contents")
    }

    pub fn macos(&self) -> PathBuf {
        self.contents().join("MacOS")
    }

    pub fn resources(&self) -> PathBuf {
        self.contents().join("Resources")
    }

    /// The `SwiftPM` product, renamed to `CFBundleExecutable`.
    pub fn shell(&self) -> PathBuf {
        self.macos().join("Applejack")
    }

    /// The `applejack` binary; `CoreLocator` looks for it beside the shell.
    pub fn core(&self) -> PathBuf {
        self.macos().join("applejack-core")
    }

    pub fn info_plist(&self) -> PathBuf {
        self.contents().join("Info.plist")
    }

    pub fn pkg_info(&self) -> PathBuf {
        self.contents().join("PkgInfo")
    }

    /// Checks that the bundle holds every member, both binaries are
    /// executable, and `PkgInfo` and `Info.plist` say what Finder expects.
    ///
    /// # Errors
    ///
    /// The first member that is missing or wrong, named.
    pub fn verify(&self) -> Result<(), String> {
        for (what, path) in [
            ("shell", self.shell()),
            ("core", self.core()),
            ("Info.plist", self.info_plist()),
            ("PkgInfo", self.pkg_info()),
        ] {
            if !path.is_file() {
                return Err(format!("{what} missing at {}", path.display()));
            }
        }
        if !self.resources().is_dir() {
            return Err(format!(
                "Resources missing at {}",
                self.resources().display()
            ));
        }
        for path in [self.shell(), self.core()] {
            if !is_executable(&path)? {
                return Err(format!("{} is not executable", path.display()));
            }
        }
        let pkg_info =
            fs::read(self.pkg_info()).map_err(|error| describe(&self.pkg_info(), &error))?;
        if pkg_info != PKG_INFO {
            return Err(format!("PkgInfo holds {pkg_info:?}, not {PKG_INFO:?}"));
        }
        let plist = fs::read_to_string(self.info_plist())
            .map_err(|error| describe(&self.info_plist(), &error))?;
        for (key, value) in [
            ("CFBundleExecutable", "Applejack"),
            ("CFBundleIdentifier", "com.applejack.app"),
            ("LSMinimumSystemVersion", "14.0"),
        ] {
            if !plist_has(&plist, key, value) {
                return Err(format!("Info.plist lacks {key} = {value}"));
            }
        }
        Ok(())
    }
}

/// Whether `plist` holds `<key>key</key>` directly followed, whitespace
/// aside, by `<string>value</string>`. A substring match on the value alone
/// would be satisfied by `CFBundleName`.
fn plist_has(plist: &str, key: &str, value: &str) -> bool {
    let marker = format!("<key>{key}</key>");
    let Some(start) = plist.find(&marker) else {
        return false;
    };
    plist[start + marker.len()..]
        .trim_start()
        .starts_with(&format!("<string>{value}</string>"))
}

/// The files that go into the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub shell: PathBuf,
    pub core: PathBuf,
    pub info_plist: PathBuf,
}

/// Builds the bundle under `<app>.tmp`, then swaps it into `layout.app`.
/// A `<app>.old` left by a run that died between the two renames is put
/// back first, so the last good bundle is never lost.
///
/// # Errors
///
/// Any copy or rename that fails, with its path.
pub fn stage(inputs: &Inputs, layout: &AppBundleLayout) -> Result<(), String> {
    let app = &layout.app;
    let tmp = sibling(app, ".tmp");
    let old = sibling(app, ".old");
    if !app.exists() && old.exists() {
        fs::rename(&old, app).map_err(|error| describe(&old, &error))?;
    }
    for (what, path) in [
        ("shell product", &inputs.shell),
        ("core binary", &inputs.core),
        ("Info.plist", &inputs.info_plist),
    ] {
        if !path.is_file() {
            return Err(format!("{what} missing at {}", path.display()));
        }
    }
    remove_tree(&tmp)?;
    let staged = AppBundleLayout::new(&tmp);
    for dir in [staged.macos(), staged.resources()] {
        fs::create_dir_all(&dir).map_err(|error| describe(&dir, &error))?;
    }
    for (from, to) in [
        (&inputs.shell, staged.shell()),
        (&inputs.core, staged.core()),
        (&inputs.info_plist, staged.info_plist()),
    ] {
        // `fs::copy` carries the mode bits, so the binaries stay executable.
        fs::copy(from, &to)
            .map_err(|error| format!("copy {} to {}: {error}", from.display(), to.display()))?;
    }
    fs::write(staged.pkg_info(), PKG_INFO).map_err(|error| describe(&staged.pkg_info(), &error))?;
    staged.verify()?;

    remove_tree(&old)?;
    if app.exists() {
        fs::rename(app, &old).map_err(|error| describe(app, &error))?;
    }
    fs::rename(&tmp, app).map_err(|error| describe(&tmp, &error))?;
    // The new bundle is in place; a leftover `.old` is recovered or removed
    // by the next run and must not fail this one.
    if let Err(error) = remove_tree(&old) {
        eprintln!("[xtask] app-bundle: left {} behind: {error}", old.display());
    }
    Ok(())
}

/// Builds the shell and the core for `profile` and assembles the bundle
/// under `build/`.
///
/// # Errors
///
/// A build that fails, or a staging error.
pub fn assemble(root: &Path, profile: Profile) -> Result<AppBundleLayout, String> {
    let configuration = profile.swift_configuration();
    run(
        swift(
            root,
            [
                "build",
                "-c",
                configuration,
                "--package-path",
                SWIFT_PACKAGE,
                "--product",
                SHELL_PRODUCT,
            ],
        ),
        "swift build",
    )?;
    let bin_path = capture(
        swift(
            root,
            [
                "build",
                "-c",
                configuration,
                "--package-path",
                SWIFT_PACKAGE,
                "--show-bin-path",
            ],
        ),
        "swift build --show-bin-path",
    )?;
    let mut cargo_args = vec!["build", "--package", "applejack"];
    if profile == Profile::Release {
        cargo_args.push("--release");
    }
    run(cargo(root, &cargo_args), "cargo build")?;

    let inputs = Inputs {
        shell: PathBuf::from(bin_path.trim()).join(SHELL_PRODUCT),
        core: target_dir(root).join(configuration).join("applejack"),
        info_plist: root.join(INFO_PLIST),
    };
    let layout = AppBundleLayout::new(root.join("build").join(profile.app_name()));
    stage(&inputs, &layout)?;
    Ok(layout)
}

/// `cargo xtask app-bundle [--debug]`.
pub fn run_command(root: &Path, args: &[String]) -> ExitCode {
    let profile = match args {
        [] => Profile::Release,
        [flag] if flag == "--debug" => Profile::Debug,
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match assemble(root, profile) {
        Ok(layout) => {
            println!("[xtask] app-bundle: {}", layout.app.display());
            for path in [
                layout.shell(),
                layout.core(),
                layout.info_plist(),
                layout.pkg_info(),
            ] {
                println!("  {}", path.strip_prefix(root).unwrap_or(&path).display());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[xtask] app-bundle: {error}");
            ExitCode::FAILURE
        }
    }
}

fn sibling(app: &Path, suffix: &str) -> PathBuf {
    let mut name = app.file_name().map(OsString::from).unwrap_or_default();
    name.push(suffix);
    app.with_file_name(name)
}

fn remove_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(describe(path, &error)),
    }
}

fn is_executable(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path).map_err(|error| describe(path, &error))?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

fn describe(path: &Path, error: &io::Error) -> String {
    format!("{}: {error}", path.display())
}

/// Where Cargo puts the core: `CARGO_TARGET_DIR` anchored at the workspace
/// root when relative, else `target/`.
fn target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |dir| root.join(dir))
}

fn cargo(root: &Path, args: &[&str]) -> Command {
    let program = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(program);
    command.args(args).current_dir(root);
    command
}

fn swift<const N: usize>(root: &Path, args: [&str; N]) -> Command {
    let mut command = Command::new("swift");
    command.args(args).current_dir(root);
    command
}

fn run(mut command: Command, what: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|error| format!("{what} failed to start: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({status})"))
    }
}

fn capture(mut command: Command, what: &str) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{what} failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!("{what} failed ({})", output.status));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("{what} wrote non-UTF-8 output: {error}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{AppBundleLayout, Inputs, PKG_INFO, plist_has, stage};

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("xtask-app-bundle-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn executable(path: &Path, content: &str) {
        fs::write(path, content).expect("write");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    fn inputs(dir: &Path, tag: &str) -> Inputs {
        let shell = dir.join(format!("ApplejackApp-{tag}"));
        let core = dir.join(format!("applejack-{tag}"));
        let plist = dir.join("Info.plist");
        executable(&shell, &format!("#!/bin/sh\necho shell {tag}\n"));
        executable(&core, &format!("#!/bin/sh\necho core {tag}\n"));
        fs::write(
            &plist,
            "<plist><dict>\n\t<key>CFBundleIdentifier</key>\n\t<string>com.applejack.app</string>\n\t<key>CFBundleName</key>\n\t<string>Applejack</string>\n\t<key>CFBundleExecutable</key>\n\t<string>Applejack</string>\n\t<key>LSMinimumSystemVersion</key>\n\t<string>14.0</string>\n</dict></plist>",
        )
        .expect("plist");
        Inputs {
            shell,
            core,
            info_plist: plist,
        }
    }

    #[test]
    fn stage_builds_the_layout_and_verifies_it() {
        let dir = scratch("layout");
        let layout = AppBundleLayout::new(dir.join("Applejack.app"));
        stage(&inputs(&dir, "a"), &layout).expect("stage");
        layout.verify().expect("verify");
        assert_eq!(fs::read(layout.pkg_info()).unwrap(), PKG_INFO);
        assert_eq!(
            fs::read_to_string(layout.shell()).unwrap(),
            "#!/bin/sh\necho shell a\n"
        );
        assert_eq!(
            fs::read_to_string(layout.core()).unwrap(),
            "#!/bin/sh\necho core a\n"
        );
        assert!(layout.resources().is_dir());
        assert!(!dir.join("Applejack.app.tmp").exists());
        assert!(!dir.join("Applejack.app.old").exists());
    }

    #[test]
    fn a_second_stage_replaces_the_bundle_in_place() {
        let dir = scratch("replace");
        let layout = AppBundleLayout::new(dir.join("Applejack.app"));
        stage(&inputs(&dir, "a"), &layout).expect("first");
        fs::write(layout.resources().join("stale"), b"x").unwrap();
        stage(&inputs(&dir, "b"), &layout).expect("second");
        assert_eq!(
            fs::read_to_string(layout.core()).unwrap(),
            "#!/bin/sh\necho core b\n"
        );
        assert!(
            !layout.resources().join("stale").exists(),
            "a rerun must not keep old files"
        );
        assert!(!dir.join("Applejack.app.old").exists());
    }

    #[test]
    fn a_missing_input_leaves_the_previous_bundle() {
        let dir = scratch("missing");
        let layout = AppBundleLayout::new(dir.join("Applejack.app"));
        stage(&inputs(&dir, "a"), &layout).expect("first");
        let mut broken = inputs(&dir, "b");
        broken.core = dir.join("nope");
        let error = stage(&broken, &layout).unwrap_err();
        assert!(error.contains("core binary missing"), "{error}");
        assert_eq!(
            fs::read_to_string(layout.core()).unwrap(),
            "#!/bin/sh\necho core a\n"
        );
    }

    #[test]
    fn an_orphaned_old_bundle_is_recovered_before_staging() {
        let dir = scratch("orphan");
        let layout = AppBundleLayout::new(dir.join("Applejack.app"));
        stage(&inputs(&dir, "a"), &layout).expect("first");
        // A run that died between the two renames leaves only `.old`.
        fs::rename(&layout.app, dir.join("Applejack.app.old")).unwrap();
        let mut broken = inputs(&dir, "b");
        broken.shell = dir.join("nope");
        assert!(stage(&broken, &layout).is_err());
        layout.verify().expect("the old bundle is back in place");
        assert_eq!(
            fs::read_to_string(layout.core()).unwrap(),
            "#!/bin/sh\necho core a\n"
        );
    }

    #[test]
    fn verify_names_what_is_wrong() {
        let dir = scratch("verify");
        let layout = AppBundleLayout::new(dir.join("Applejack.app"));
        assert!(layout.verify().unwrap_err().contains("shell missing"));
        stage(&inputs(&dir, "a"), &layout).expect("stage");
        fs::set_permissions(layout.core(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(layout.verify().unwrap_err().contains("not executable"));
        fs::write(layout.pkg_info(), b"XXXX????").unwrap();
        fs::set_permissions(layout.core(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(layout.verify().unwrap_err().contains("PkgInfo"));
        fs::write(layout.pkg_info(), PKG_INFO).unwrap();
        // `CFBundleName` alone must not pass for `CFBundleExecutable`.
        fs::write(
            layout.info_plist(),
            "<plist><dict><key>CFBundleName</key><string>Applejack</string><key>CFBundleIdentifier</key><string>com.applejack.app</string><key>LSMinimumSystemVersion</key><string>14.0</string></dict></plist>",
        )
        .unwrap();
        assert!(
            layout
                .verify()
                .unwrap_err()
                .contains("CFBundleExecutable = Applejack")
        );
    }

    #[test]
    fn plist_has_matches_key_and_value_pairs_only() {
        let plist = "<key>CFBundleName</key>\n\t<string>Applejack</string>\n\t<key>CFBundleExecutable</key><string>Other</string>";
        assert!(plist_has(plist, "CFBundleName", "Applejack"));
        assert!(!plist_has(plist, "CFBundleExecutable", "Applejack"));
        assert!(!plist_has(plist, "CFBundleIdentifier", "com.applejack.app"));
    }
}
