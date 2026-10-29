//! Steam layer: authentication, Keychain-backed refresh-token storage, the
//! owned-license list, `.acf` and `.vdf` handling, and the advisory locks that
//! serialize applejack processes around the shared library.
//!
//! This crate accepts and returns plain structs and never depends on
//! `applejack-config`. Everything that talks to Steam sits behind
//! [`auth::SteamConnection`]; [`live::SteamroomConnection`] is the only
//! implementation that opens a network connection, and nothing in this crate
//! calls it at test time.

pub mod acf;
pub mod auth;
mod error;
pub mod keychain;
pub mod live;
pub mod lock;
pub mod vdf;

pub use acf::{AcfWriter, AppManifest, InstalledDepot};
pub use auth::{AuthError, AuthMode, Authenticator, LoginStep, SteamConnection};
pub use error::Error;
pub use keychain::{InMemoryStore, RefreshToken, RefreshTokenStore, SecurityFrameworkStore};
pub use lock::{AppLock, LibraryLock, Lock, LockDir, Mode};
pub use vdf::{Document, Node, Value};
