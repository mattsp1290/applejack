//! Local development commands, invoked with `cargo xtask`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use applejack_manifest::Manifest;

mod app_bundle;
mod bundle_toml;
mod closure;
mod denylist;
mod engine_payload;
mod pins;
mod self_contained;

const USAGE: &str = "usage: cargo xtask <check|manifest-verify|pins-verify|engine-payload <mode>|app-bundle [--debug]>";

/// One quality-gate step. Sibling work packages append rows to `CHECK_STEPS`.
struct Step {
    name: &'static str,
    command: fn(&Path) -> Command,
    /// Runs before `command`; an `Err` fails the step with that message.
    precondition: Option<Precondition>,
}

type Precondition = fn(&Path) -> Result<(), String>;

const SWIFT_PACKAGE: &str = "apps/Applejack";

const CHECK_STEPS: &[Step] = &[
    Step {
        name: "fmt",
        command: |root| cargo(root, ["fmt", "--all", "--check"]),
        precondition: None,
    },
    Step {
        name: "clippy",
        command: |root| {
            cargo(
                root,
                [
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            )
        },
        precondition: None,
    },
    Step {
        name: "test",
        command: |root| cargo(root, ["test", "--workspace"]),
        precondition: None,
    },
    Step {
        name: "doc",
        command: |root| {
            let mut command = cargo(root, ["doc", "--workspace", "--no-deps"]);
            command.env("RUSTDOCFLAGS", "-D warnings");
            command
        },
        precondition: None,
    },
    Step {
        name: "manifest-verify",
        command: |root| cargo(root, ["xtask", "manifest-verify"]),
        precondition: None,
    },
    Step {
        name: "pins-verify",
        command: |root| cargo(root, ["xtask", "pins-verify"]),
        precondition: None,
    },
    Step {
        name: "core-build",
        command: |root| cargo(root, ["build", "--package", "applejack"]),
        precondition: None,
    },
    Step {
        name: "swift-build",
        command: |root| swift(root, ["build", "--package-path", SWIFT_PACKAGE]),
        precondition: None,
    },
    Step {
        name: "swift-test",
        command: |root| {
            let mut command = swift(root, ["test", "--package-path", SWIFT_PACKAGE]);
            command.env("APPLEJACK_CORE_BIN", core_bin(root));
            command
        },
        // The Swift suite skips the real-binary test when the binary is absent;
        // checking here keeps that skip from hiding a missing build during `check`.
        precondition: Some(|root| {
            let binary = core_bin(root);
            if binary.is_file() {
                Ok(())
            } else {
                Err(format!(
                    "core binary missing at {}; expected `cargo build --package applejack` to produce it",
                    binary.display()
                ))
            }
        }),
    },
    Step {
        name: "app-bundle",
        command: |root| cargo(root, ["xtask", "app-bundle", "--debug"]),
        precondition: None,
    },
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [command] if command == "check" => check(&workspace_root()),
        [command] if command == "manifest-verify" => manifest_verify(&workspace_root()),
        [command] if command == "pins-verify" => engine_payload::pins_verify(&workspace_root()),
        [command, rest @ ..] if command == "engine-payload" => {
            engine_payload::run(&workspace_root(), rest)
        }
        [command, rest @ ..] if command == "app-bundle" => {
            app_bundle::run_command(&workspace_root(), rest)
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn check(root: &Path) -> ExitCode {
    for step in CHECK_STEPS {
        let mut command = (step.command)(root);
        let rendered = render(&command);
        if let Some(precondition) = step.precondition
            && let Err(message) = precondition(root)
        {
            eprintln!("[xtask] {} failed: {message}\n  {rendered}", step.name);
            return ExitCode::FAILURE;
        }
        println!("[xtask] {}: {rendered}", step.name);
        let status = match command.status() {
            Ok(status) => status,
            Err(error) => {
                eprintln!(
                    "[xtask] {} failed to start: {error}\n  {rendered}",
                    step.name
                );
                return ExitCode::FAILURE;
            }
        };
        if !status.success() {
            eprintln!("[xtask] {} failed ({status})\n  {rendered}", step.name);
            return ExitCode::FAILURE;
        }
    }
    println!("[xtask] check passed");
    ExitCode::SUCCESS
}

fn manifest_verify(root: &Path) -> ExitCode {
    let path = root.join("engines").join("manifest.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("[xtask] manifest-verify: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let manifest =
        Manifest::parse(&text).and_then(|manifest| manifest.validate().map(|()| manifest));
    match manifest {
        Ok(manifest) => {
            println!(
                "[xtask] manifest-verify: {} ok (engines: {}, tools: {}, probes: {})",
                path.display(),
                manifest.engine.len(),
                manifest.tool.len(),
                manifest.probe.len()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[xtask] manifest-verify: {}: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

/// The debug binary `core-build` produces, wherever Cargo's target directory is.
fn core_bin(root: &Path) -> PathBuf {
    // `root.join` keeps an absolute `CARGO_TARGET_DIR` unchanged and anchors a
    // relative one at the workspace root, which is where the cargo steps run.
    let target =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |dir| root.join(dir));
    target.join("debug").join("applejack")
}

fn cargo<const N: usize>(root: &Path, args: [&str; N]) -> Command {
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

fn render(command: &Command) -> String {
    let envs = command.get_envs().filter_map(|(key, value)| {
        let value = value?.to_string_lossy();
        // Quote so the printed command can be pasted back into a shell.
        let value = if value.contains(char::is_whitespace) {
            format!("'{value}'")
        } else {
            value.into_owned()
        };
        Some(format!("{}={value} ", key.to_string_lossy()))
    });
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{} {args}",
        envs.collect::<String>(),
        command.get_program().to_string_lossy()
    )
}
