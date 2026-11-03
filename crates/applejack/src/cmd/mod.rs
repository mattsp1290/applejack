//! Subcommand implementations. Each module exposes a clap `Args` type and a
//! `run` function returning the process exit code.

pub mod app;
pub mod config;
pub mod engine;
pub mod install;
pub mod prefix;
pub mod runtime;
pub mod serve;
pub mod session;
pub mod steam;

use std::io::{self, Write};
use std::process::ExitCode;

use applejack_core::CoreError;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// What the shell reads as "show the login": no silent sign-in worked.
const EXIT_STEAM_LOGIN: u8 = 3;

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

/// Prints a command's failure, and picks the exit code a script can act on.
#[must_use]
pub fn fail(command: &str, error: &CoreError) -> ExitCode {
    let _ = writeln!(io::stderr(), "applejack {command}: {error}");
    match error {
        CoreError::SteamLoginRequired { .. } | CoreError::SteamRefreshFailed(_) => {
            ExitCode::from(EXIT_STEAM_LOGIN)
        }
        _ => ExitCode::FAILURE,
    }
}

/// Runs `first` on the first `SIGINT` or `SIGTERM`, so the owner of the work
/// can finish and close it. A second one stops waiting for that.
pub fn on_signal(first: impl FnOnce() + Send + 'static) {
    let mut signals = match Signals::new([SIGINT, SIGTERM]) {
        Ok(signals) => signals,
        Err(error) => {
            let _ = writeln!(io::stderr(), "applejack: no signal handling: {error}");
            return;
        }
    };
    std::thread::spawn(move || {
        let mut received = signals.forever();
        if received.next().is_some() {
            std::thread::spawn(first);
        }
        if let Some(signal) = received.next() {
            std::process::exit(128 + signal);
        }
    });
}
