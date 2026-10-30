//! `applejack serve --stdio`: the JSON-RPC loop the app shell drives.

use applejack_core::rpc::{self, CoreHandler, Logged};
use clap::Args;
use std::io;
use std::process::ExitCode;

const USAGE: &str = "usage: applejack serve --stdio [--log]";

/// `EX_IOERR` from sysexits(3): stdin or stdout failed mid-session.
const EXIT_IO_ERROR: u8 = 74;

#[derive(Args, Clone, Copy)]
pub struct ServeArgs {
    /// Speak JSON-RPC 2.0 over stdin and stdout, one message per line
    #[arg(long)]
    stdio: bool,
    /// Write one line per request and response to stderr (steam.login.* redacted)
    #[arg(long)]
    log: bool,
}

/// Exit 0 on EOF after a good `hello`, 65 on a protocol mismatch, 2 without
/// `--stdio`. Nothing but responses is written to stdout.
pub fn run(args: ServeArgs) -> ExitCode {
    if !args.stdio {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let stdin = io::stdin().lock();
    let stdout = io::stdout().lock();
    let outcome = if args.log {
        rpc::serve(stdin, stdout, &mut Logged::new(CoreHandler, io::stderr()))
    } else {
        rpc::serve(stdin, stdout, &mut CoreHandler)
    };
    match outcome {
        Ok(status) => ExitCode::from(status.code()),
        Err(error) => {
            eprintln!("applejack serve: {error}");
            ExitCode::from(EXIT_IO_ERROR)
        }
    }
}
