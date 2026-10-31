//! The logged-in half of the Steam layer. [`SteamConnection`](crate::auth::SteamConnection)
//! keeps the auth flow; `logon` hands back a [`SteamSession`] whose methods
//! take `&self`, so a keepalive and any number of requests share it.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use steamroom::depot::DepotKey;
use steamroom::depot::manifest::DepotManifest;
use steamroom_client::download::ChunkFetcher;

use crate::auth::{AuthError, License};

/// One app's PICS product info. `text` is the text `KeyValues` buffer, or
/// `None` when Steam answered without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub appid: u32,
    pub change_number: u32,
    pub text: Option<String>,
}

/// What an account's licenses grant, from their packages' `appids` and
/// `depotids`. A depot is available when either lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageGrants {
    pub appids: BTreeSet<u32>,
    pub depot_ids: BTreeSet<u32>,
}

/// The calls a logged-in session makes. A session stays alive on its own for
/// as long as it exists: the implementation sends heartbeats while a call is
/// waiting, so no caller schedules them.
pub trait SteamSession: Send + Sync {
    /// The chunk source [`chunks`](Self::chunks) returns.
    type Chunks: ChunkFetcher + 'static;

    /// The interval the CM asked for in the logon response; `None` when it
    /// did not say.
    fn heartbeat_interval(&self) -> Option<Duration>;

    /// The owned licenses. Safe to call at any time and more than once:
    /// answers from the latest `CMsgClientLicenseList`.
    fn license_list(&self) -> impl Future<Output = Result<Vec<License>, AuthError>> + Send;

    /// What the given licenses grant.
    fn package_grants(
        &self,
        licenses: &[License],
    ) -> impl Future<Output = Result<PackageGrants, AuthError>> + Send;

    /// PICS product info for `appids`. Steam omits apps it does not know.
    fn app_info(
        &self,
        appids: &[u32],
    ) -> impl Future<Output = Result<Vec<AppInfo>, AuthError>> + Send;

    /// The AES key for a depot's chunks and file names. Never log it.
    fn depot_key(
        &self,
        appid: u32,
        depot_id: u32,
    ) -> impl Future<Output = Result<DepotKey, AuthError>> + Send;

    /// A depot manifest, file names still encrypted. `Ok(None)` means Steam
    /// no longer serves this manifest id.
    fn manifest(
        &self,
        appid: u32,
        depot_id: u32,
        manifest_id: u64,
        branch: &str,
    ) -> impl Future<Output = Result<Option<DepotManifest>, AuthError>> + Send;

    /// A source for this depot's encrypted chunks.
    fn chunks(
        &self,
        appid: u32,
        depot_id: u32,
    ) -> impl Future<Output = Result<Arc<Self::Chunks>, AuthError>> + Send;
}
