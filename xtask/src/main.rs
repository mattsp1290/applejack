//! Local development commands, invoked with `cargo xtask`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use applejack_manifest::Manifest;

const USAGE: &str = "usage: cargo xtask <check|manifest-verify>";

/// One quality-gate step. Sibling work packages append rows to `CHECK_STEPS`.
struct Step {
    name: &'static str,
    command: fn(&Path) -> Command,
}

const CHECK_STEPS: &[Step] = &[
    Step {
        name: "fmt",
        command: |root| cargo(root, ["fmt", "--all", "--check"]),
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
    },
    Step {
        name: "test",
        command: |root| cargo(root, ["test", "--workspace"]),
    },
    Step {
        name: "doc",
        command: |root| {
            let mut command = cargo(root, ["doc", "--workspace", "--no-deps"]);
            command.env("RUSTDOCFLAGS", "-D warnings");
            command
        },
    },
    Step {
        name: "manifest-verify",
        command: |root| cargo(root, ["xtask", "manifest-verify"]),
    },
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [command] if command == "check" => check(&workspace_root()),
        [command] if command == "manifest-verify" => manifest_verify(&workspace_root()),
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

fn cargo<const N: usize>(root: &Path, args: [&str; N]) -> Command {
    let program = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(program);
    command.args(args).current_dir(root);
    command
}

fn render(command: &Command) -> String {
    let envs = command.get_envs().filter_map(|(key, value)| {
        Some(format!(
            "{}={} ",
            key.to_string_lossy(),
            value?.to_string_lossy()
        ))
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
