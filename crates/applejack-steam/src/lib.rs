//! Steam layer: authentication, Keychain-backed refresh-token storage, the
//! owned-app library, Windows depot install, update, and verify, `.acf` and
//! `.vdf` handling, and the advisory locks that serialize applejack processes
//! around the shared library.
//!
//! This crate accepts and returns plain structs and never depends on
//! `applejack-config` or `applejack-engine`. Everything that talks to Steam
//! sits behind [`auth::SteamConnection`] and [`session::SteamSession`];
//! [`live`] holds the only implementations that open a network connection,
//! and nothing in this crate calls them at test time.
//!
//! # Token rules
//!
//! The refresh token lives only in a Keychain generic-password item (service
//! `applejack`, account `steam-refresh-token`) written through the Security
//! framework; the `security` command line tool is never invoked. No token is
//! ever passed on a process command line: this crate spawns no process at
//! all, so it constructs no `Command` from `std::process` or `tokio::process`
//! (`clippy.toml` lists both under `disallowed-types`, and
//! `tests/no_token_argv.rs` greps for them). [`keychain::RefreshToken`],
//! [`auth::AccessToken`], and [`auth::Password`] redact themselves in `Debug`
//! and are readable only through `expose()`. `docs/steam-layer.md` under
//! "Token handling rules" is the full list.

pub mod acf;
pub mod auth;
pub mod depot;
mod error;
pub mod keychain;
pub mod library;
pub mod live;
pub mod lock;
pub mod session;
pub mod vdf;

pub use acf::{AcfWriter, AppManifest, InstalledDepot};
pub use auth::{
    AccessToken, AuthError, AuthMode, Authenticator, Credentials, GuardFactor, License, LoginStep,
    Password, SteamConnection, Tokens,
};
pub use depot::{
    ClientRunning, DepotInstaller, DownloadRecord, InstallOptions, InstallOutcome, InstallPlan,
    InstallProgress, LibraryPaths, VerifyReport, verify_install,
};
pub use error::Error;
pub use keychain::{InMemoryStore, RefreshToken, RefreshTokenStore, SecurityFrameworkStore};
pub use library::{
    DepotFilter, DepotInfo, DepotSelection, Library, ManifestRef, OwnedApp, PlannedDepot,
};
pub use lock::{AppLock, Holder, LibraryLock, Lock, LockDir, Mode};
pub use session::{AppInfo, PackageGrants, SteamSession};
pub use vdf::{Document, Node, Value};
