//! Steam layer: authentication, Keychain-backed refresh-token storage, the
//! owned-license list, `.acf` and `.vdf` handling, and the advisory locks that
//! serialize applejack processes around the shared library.
//!
//! This crate accepts and returns plain structs and never depends on
//! `applejack-config`. Everything that talks to Steam sits behind
//! [`auth::SteamConnection`]; [`live::SteamroomConnection`] is the only
//! implementation that opens a network connection, and nothing in this crate
//! calls it at test time.
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
mod error;
pub mod keychain;
pub mod live;
pub mod lock;
pub mod vdf;

pub use acf::{AcfWriter, AppManifest, InstalledDepot};
pub use auth::{
    AccessToken, AuthError, AuthMode, Authenticator, Credentials, GuardFactor, License, LoginStep,
    Password, SteamConnection, Tokens,
};
pub use error::Error;
pub use keychain::{InMemoryStore, RefreshToken, RefreshTokenStore, SecurityFrameworkStore};
pub use lock::{AppLock, Holder, LibraryLock, Lock, LockDir, Mode};
pub use vdf::{Document, Node, Value};
