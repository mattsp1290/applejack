//! `applejack steam login|logout|library`. The password comes from the
//! terminal with echo off, and no secret is ever an argument. A code prompt
//! keeps polling: approving in the Steam mobile app finishes the login just
//! as typing the code does, and whichever comes second is dropped.

use std::fs::File;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use applejack_core::rpc::wire::ErrorCode;
use applejack_core::steam::{
    AuthMode, Credentials, GuardFactor, InstallState, LibraryEntry, LiveSteam, LoginFlow,
    LoginStep, Password, SteamLibraryResult, refuse_blank_account,
};
use applejack_core::{Core, CoreError};
use clap::{Args, Subcommand};
use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

/// What a shell reads as a command ended by control-C.
const EXIT_INTERRUPTED: i32 = 130;

#[derive(Args)]
pub struct SteamArgs {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Sign in once; the refresh token goes to the Keychain
    Login {
        /// Draw a QR code in the terminal for the Steam mobile app to scan,
        /// with the URL under it, instead of asking for the account name and
        /// password
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

fn say(message: &str) {
    let _ = writeln!(io::stderr(), "{message}");
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

/// From here to the end of the process, a signal puts the terminal back and
/// exits with the status a shell reports for that signal, `128 + signal`. The
/// parent sees an exit, not a death by signal. `rpassword` clears echo,
/// canonical mode, and `ISIG`, reads control-C as a character, and raises
/// `SIGINT` before its own restore, so with the default action the terminal
/// stays that way. The handler stays for good because a signal-hook handler
/// is not undone: the polling that follows must still answer control-C. With
/// no terminal there is nothing to restore, and the prompt reports that
/// itself.
fn restore_terminal_on_signal() -> Result<(), CoreError> {
    let Ok(terminal) = File::options().read(true).write(true).open("/dev/tty") else {
        return Ok(());
    };
    let Ok(modes) = tcgetattr(&terminal) else {
        return Ok(());
    };
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])
        .map_err(|error| CoreError::Message(format!("cannot handle signals: {error}")))?;
    thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            let _ = tcsetattr(&terminal, OptionalActions::Now, &modes);
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

fn ask_credentials() -> Result<Credentials, CoreError> {
    let account_name = ask_line("Steam account name: ")?;
    refuse_blank_account(&account_name)?;
    restore_terminal_on_signal()?;
    let password = match rpassword::prompt_password("Steam password: ") {
        Ok(password) => password,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            say("");
            std::process::exit(EXIT_INTERRUPTED);
        }
        Err(error) => {
            return Err(CoreError::Message(format!(
                "cannot read the password: {error}"
            )));
        }
    };
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

/// The line above the drawing. `first_challenge` is false once a QR challenge
/// has been on screen: Steam rotates it about every 30 s, and the user should
/// know the old one is dead.
fn qr_intro(first_challenge: bool) -> &'static str {
    if first_challenge {
        "Scan this QR code with the Steam mobile app:"
    } else {
        "The challenge rotated; scan this one instead:"
    }
}

/// What a `QrReady` step prints: the intro, the drawing when the payload fits
/// a QR code, and the URL for a terminal that cannot show the blocks.
fn qr_lines(payload: &str, first_challenge: bool) -> String {
    let intro = qr_intro(first_challenge);
    match crate::qr::half_blocks(payload) {
        Some(drawing) => format!("{intro}\n{drawing}\n{payload}"),
        None => format!("{intro}\n{payload}"),
    }
}

/// The core's expiry message names no command, since the shell shows the same
/// text beside its Retry button; the command is this one.
fn name_the_command(error: CoreError) -> CoreError {
    match error {
        CoreError::SteamQrExpired { .. } => {
            CoreError::Message(format!("{error}; run `applejack steam login --qr` again"))
        }
        other => other,
    }
}

fn show(step: &LoginStep, first_challenge: bool) {
    match step {
        LoginStep::QrReady { payload } => say(&qr_lines(payload, first_challenge)),
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceConfirmation,
        } => say("Approve the sign-in in the Steam mobile app."),
        LoginStep::GuardRequired {
            factor: GuardFactor::DeviceCode,
        } => say("Type the Steam Guard code and press return, or approve in the Steam mobile app."),
        LoginStep::GuardRequired {
            factor: GuardFactor::EmailCode,
        } => say("Type the Steam Guard code from your email and press return."),
        LoginStep::Waiting => say("Waiting for Steam to finish the sign-in."),
        LoginStep::Done { .. } => {}
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
            let first_challenge = shown
                .as_ref()
                .is_none_or(|previous| !matches!(previous, LoginStep::QrReady { .. }));
            show(&step, first_challenge);
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
            Err(error) => return Err(name_the_command(error)),
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

    const CHALLENGE: &str = "https://s.team/q/1/1234567890123456";

    #[test]
    fn a_rotation_says_the_old_code_is_dead_and_the_first_challenge_does_not() {
        let first = qr_lines(CHALLENGE, true);
        let rotated = qr_lines(CHALLENGE, false);
        assert!(
            first.starts_with("Scan this QR code with the Steam mobile app:\n"),
            "{first}"
        );
        assert!(!first.contains("rotated"), "{first}");
        assert!(
            rotated.starts_with("The challenge rotated; scan this one instead:\n"),
            "{rotated}"
        );
        assert!(!rotated.contains("Scan this QR code"), "{rotated}");
        for lines in [&first, &rotated] {
            assert!(lines.ends_with(&format!("\n{CHALLENGE}")), "{lines}");
            assert!(lines.lines().count() > 2, "a drawing sits between: {lines}");
        }
        assert_eq!(
            qr_lines(&"x".repeat(3000), true).lines().count(),
            2,
            "over capacity, the intro and the URL alone"
        );
    }

    #[test]
    fn the_expiry_gains_this_command_and_other_errors_pass_through() {
        let error = name_the_command(CoreError::SteamQrExpired { seconds: 125 });
        assert_eq!(
            error.to_string(),
            "the QR challenge expired after 125 s without a scan; run `applejack steam login --qr` again"
        );
        let other = name_the_command(CoreError::NoSuchLogin);
        assert!(matches!(other, CoreError::NoSuchLogin), "{other}");
    }
}
