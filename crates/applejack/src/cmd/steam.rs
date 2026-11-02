//! `applejack steam login|logout|library`. The password comes from the
//! terminal with echo off, and no secret is ever an argument. A code prompt
//! keeps polling: approving in the Steam mobile app finishes the login just
//! as typing the code does, and whichever comes second is dropped.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use applejack_core::rpc::wire::ErrorCode;
use applejack_core::steam::{
    AuthMode, Credentials, GuardFactor, InstallState, LibraryEntry, LiveSteam, LoginFlow,
    LoginStep, Password, SteamLibraryResult,
};
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct SteamArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Sign in once; the refresh token goes to the Keychain
    Login {
        /// Print a challenge for the Steam mobile app to scan, instead of
        /// asking for the account name and password
        #[arg(long)]
        qr: bool,
    },
    /// Delete the token and forget the cached library. Installed titles stay
    Logout,
    /// List the owned titles with a Windows depot
    Library {
        /// Print as JSON
        #[arg(long)]
        json: bool,
        /// Read the library from Steam, not the cache
        #[arg(long)]
        refresh: bool,
    },
}

pub fn run(args: &SteamArgs) -> ExitCode {
    let outcome = match args.action {
        Action::Login { qr } => login(qr),
        Action::Logout => logout(),
        Action::Library { json, refresh } => library(json, refresh),
    };
    match outcome {
        Ok(text) => super::emit("steam", &text),
        Err(error) => super::fail("steam", &error),
    }
}

fn say(line: &str) {
    let _ = writeln!(io::stderr(), "{line}");
}

fn ask_line(prompt: &str) -> Result<String, CoreError> {
    let mut err = io::stderr();
    let _ = write!(err, "{prompt}");
    let _ = err.flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| CoreError::Message(format!("cannot read the terminal: {error}")))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn ask_credentials() -> Result<Credentials, CoreError> {
    let account_name = ask_line("Steam account name: ")?;
    let password = rpassword::prompt_password("Steam password: ")
        .map_err(|error| CoreError::Message(format!("cannot read the password: {error}")))?;
    Ok(Credentials {
        account_name,
        password: Password::new(password),
    })
}

/// Lines from stdin as they come. The thread is left in its read when the
/// login ends first; the process exiting ends it. That is why codes are read
/// with echo on: a process that leaves mid-read with echo off leaves the
/// terminal that way.
fn read_codes() -> Receiver<String> {
    let (lines, codes) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    codes
}

fn show(step: &LoginStep) {
    match step {
        LoginStep::QrReady { payload } => {
            say(&format!(
                "Scan this as a QR code with the Steam mobile app:\n{payload}"
            ));
        }
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceConfirmation,
        } => say("Approve the sign-in in the Steam mobile app."),
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceCode,
        } => say("Type the Steam Guard code and press return, or approve in the Steam mobile app."),
        LoginStep::GuardRequired {
            factor: GuardFactor::EmailCode,
        } => say("Type the Steam Guard code from your email and press return."),
        LoginStep::Waiting | LoginStep::Done { .. } => {}
    }
}

fn login(qr: bool) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    let (mode, credentials) = if qr {
        (AuthMode::Qr, None)
    } else {
        (AuthMode::Credentials, Some(ask_credentials()?))
    };
    let host = Arc::new(LiveSteam::default());
    let mut flow = LoginFlow::start(&mut core, host, mode, credentials.as_ref())?;
    drop(credentials);
    let codes = read_codes();
    let mut shown = None;
    loop {
        let step = flow.step().clone();
        if let LoginStep::Done { account_name } = &step {
            return Ok(format!("signed in as {account_name}"));
        }
        if shown.as_ref() != Some(&step) {
            show(&step);
            shown = Some(step);
        }
        let interval = flow.poll_interval();
        let advanced = match codes.recv_timeout(interval) {
            Ok(line) => flow.advance(&mut core, Some(&line)),
            Err(RecvTimeoutError::Timeout) => flow.advance(&mut core, None),
            // No more lines: only an approval can finish this.
            Err(RecvTimeoutError::Disconnected) => {
                thread::sleep(interval);
                flow.advance(&mut core, None)
            }
        };
        match advanced {
            Ok(_) => {}
            Err(error) if error.code() == ErrorCode::SteamGuardRejected => {
                say(&format!("{error}; type it again."));
            }
            Err(error) => return Err(error),
        }
    }
}

fn logout() -> Result<String, CoreError> {
    Core::from_env()?.steam_logout(&LiveSteam::default())?;
    Ok("signed out; installed titles stay".to_owned())
}

fn library(json: bool, refresh: bool) -> Result<String, CoreError> {
    let mut core = Core::from_env()?;
    let mut listing = core.steam_library_cached()?;
    if refresh || listing.fetched_at.is_none() {
        listing = core.steam_library_refresh(&LiveSteam::default())?;
    }
    if json {
        return Ok(serde_json::to_string_pretty(&listing)?);
    }
    Ok(render(&listing))
}

fn state_name(state: InstallState) -> &'static str {
    match state {
        InstallState::Installed => "installed",
        InstallState::NotInstalled => "-",
        InstallState::UpdateAvailable => "update",
    }
}

fn row(app: &LibraryEntry) -> String {
    format!(
        "{:>8}  {:>7.1} GB  {:<7}  {:<9}  {}",
        app.appid,
        size_gb(app.size_bytes),
        app.drm.as_str(),
        state_name(app.install_state),
        app.name
    )
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a display figure, one decimal of a size well under 2^52 bytes"
)]
fn size_gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

fn render(listing: &SteamLibraryResult) -> String {
    if listing.apps.is_empty() {
        return "no owned title has a Windows depot".to_owned();
    }
    let header = format!(
        "{:>8}  {:>10}  {:<7}  {:<9}  name",
        "appid", "size", "drm", "state"
    );
    let rows = listing.apps.iter().map(row);
    std::iter::once(header)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use applejack_core::config::Drm;

    #[test]
    fn a_library_row_lines_up_and_names_its_state() {
        let line = row(&LibraryEntry {
            appid: 480,
            name: "Example".to_owned(),
            installdir: "Example".to_owned(),
            size_bytes: 2_500_000_000,
            drm: Drm::Unknown,
            default_branch: "public".to_owned(),
            install_state: InstallState::UpdateAvailable,
        });
        assert_eq!(line, "     480      2.5 GB  unknown  update     Example");
    }
}
