# applejack

applejack is a macOS Windows Steam game runner: an engine-agnostic launcher
for Apple Silicon that downloads a user's Windows Steam titles, builds a Wine
prefix, launches the game through a swappable engine (x86-64 Wine under
Rosetta 2 now, native ARM64 Wine plus a Darwin FEX later), and owns the
durable parts around it: the Steam layer, the per-game configuration
database, controller handling, and the signing and notarization pipeline.

## Status

Scaffold only. The Cargo workspace, the CLI surface, and the quality gate
exist; nothing runs games yet. Every subcommand except `doctor` exits with
code 69 (`EX_UNAVAILABLE`) until its work package lands.

## Requirements

An Apple Silicon Mac. rustup, which installs the pinned toolchain from
`rust-toolchain.toml` on first use. Xcode 26.2 or newer with its command
line tools. Network access for the first build so Cargo can fetch crates.

## Build and run

```sh
cargo build
cargo run -p applejack -- doctor
cargo run -p applejack -- doctor --json
cargo xtask check
```

`doctor` reports host facts without network access: macOS version, chip
architecture, Rosetta presence, free disk under
`~/Library/Application Support/applejack`, Screen Recording permission state
(reported as `unknown` for now), and the number of Developer ID Application
identities.

## Verification

`cargo xtask check` runs `cargo fmt --all --check`, Clippy with `-D warnings`
and the workspace lints, `cargo test --workspace`, rustdoc with
`RUSTDOCFLAGS=-D warnings`, `cargo xtask manifest-verify` on
`engines/manifest.toml`, then the Swift package build and tests with
`APPLEJACK_CORE_BIN` pointing at the freshly built core binary. It stops at
the first failure. CI runs the same
command on a GitHub-hosted Apple Silicon runner (`macos-latest`) on every push
to `main` and on every pull request; see `.github/workflows/check.yml`.
