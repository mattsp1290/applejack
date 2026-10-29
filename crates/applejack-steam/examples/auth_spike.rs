//! Gate G2 spike: log in to Steam with `steamroom`, obtain a refresh token,
//! log on again with it, and decode the owned-license list on that
//! connection. Runs only with `APPLEJACK_LIVE_STEAM=1`; see
//! `docs/steam-layer.md` for how to run it and record the outcome.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use applejack_steam::SteamConnection;
use applejack_steam::auth::{
    AuthMode, Authenticator, Credentials, GuardFactor, LoginStep, Password,
};
use applejack_steam::keychain::InMemoryStore;
use applejack_steam::live::{STEAMROOM_CLIENT_VERSION, STEAMROOM_VERSION, SteamroomConnection};

const USAGE: &str = "\
auth_spike: Steam authentication spike for gate G2 (not run; nothing was contacted).

Run with your own Steam account:

  APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example auth_spike
  APPLEJACK_LIVE_STEAM=1 cargo run -p applejack-steam --example auth_spike -- --credentials

QR mode prints the challenge URL to encode as a QR code for the Steam mobile app.
Credentials mode reads the account name from stdin and the password from the
terminal without echo. The spike stores the refresh token in memory only, logs on
with it once, prints the number of licenses on the account, and never prints the
token.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    if std::env::var("APPLEJACK_LIVE_STEAM").as_deref() != Ok("1") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let mode = if std::env::args().skip(1).any(|arg| arg == "--credentials") {
        AuthMode::Credentials
    } else {
        AuthMode::Qr
    };
    match run(mode).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("auth_spike: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(mode: AuthMode) -> Result<(), Box<dyn std::error::Error>> {
    println!("steamroom {STEAMROOM_VERSION}, steamroom-client {STEAMROOM_CLIENT_VERSION}");
    let credentials = match mode {
        AuthMode::Qr => None,
        AuthMode::Credentials => Some(Credentials {
            account_name: prompt_line("Steam account name: ")?,
            password: Password::new(rpassword::prompt_password("Steam password: ")?),
        }),
    };

    let store = InMemoryStore::default();
    let mut connection = SteamroomConnection::new();
    let account_name = {
        let mut auth = Authenticator::new(&mut connection, &store);
        let mut step = auth.begin(mode, credentials.as_ref()).await?;
        loop {
            step = match step {
                LoginStep::QrReady { payload } => {
                    println!("Scan this URL as a QR code with the Steam mobile app:\n{payload}");
                    auth.finish().await?
                }
                LoginStep::GuardRequired {
                    factor: GuardFactor::DeviceConfirmation,
                } => {
                    println!("Approve the sign-in in the Steam mobile app.");
                    auth.finish().await?
                }
                LoginStep::GuardRequired { factor } => {
                    let code = prompt_line(&format!("Steam Guard code ({factor:?}): "))?;
                    auth.submit_guard_code(code.trim()).await?
                }
                LoginStep::Done { account_name } => break account_name,
            };
        }
    };
    println!("login ok for account `{account_name}`; refresh token held in memory");

    {
        let mut auth = Authenticator::new(&mut connection, &store);
        auth.refresh(&account_name).await?;
    }
    println!("refresh logon ok");

    if let Some(interval) = connection.heartbeat_interval() {
        println!("heartbeat interval: {}s", interval.as_secs());
    }
    let licenses = connection.license_list().await?;
    println!("licenses on account: {}", licenses.len());
    println!("gate G2: green (record this in docs/steam-layer.md)");
    Ok(())
}

fn prompt_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}
