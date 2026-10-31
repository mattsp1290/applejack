//! Subcommand implementations. Each module exposes a clap `Args` type and a
//! `run` function returning the process exit code.

pub mod config;
pub mod engine;
pub mod prefix;
pub mod serve;

use std::io::{self, Write};
use std::process::ExitCode;

/// Writes `text` to stdout. A reader that closes the pipe early is not an
/// error for a report command, so `BrokenPipe` exits 0.
pub fn emit(command: &str, text: &str) -> ExitCode {
    let mut out = io::stdout().lock();
    match writeln!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("applejack {command}: {error}");
            ExitCode::FAILURE
        }
    }
}
