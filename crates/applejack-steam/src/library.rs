//! The owned-app library: licenses to packages to appids to PICS product
//! info, reduced to plain values. Persisting them (the `apps` table) belongs
//! to the caller, since this crate does not depend on `applejack-config`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use steamroom::types::key_value::{KeyValue, KvValue};

use crate::Error;
use crate::session::{AppInfo, SteamSession};

/// The `oslist` value for Windows.
pub const WINDOWS: &str = "windows";

/// The branch every app has unless Steam says otherwise.
pub const DEFAULT_BRANCH: &str = "public";

/// `OwnedApp::drm` until the config database says otherwise: DRM cannot be
/// inferred from Steam.
pub const DRM_UNKNOWN: &str = "unknown";

const X86_64: u32 = 64;

/// One branch's entry under a depot's `manifests`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRef {
    pub manifest_id: u64,
    pub size: u64,
}

/// One numbered entry under an app's `depots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepotInfo {
    pub depot_id: u32,
    /// `config/oslist`, lower-cased. Empty means every platform.
    pub oslist: Vec<String>,
    /// `config/osarch`: `32`, `64`, or absent for either.
    pub osarch: Option<u32>,
    pub language: Option<String>,
    pub low_violence: bool,
    /// The DLC that must be owned for this depot to be downloadable.
    pub dlc_appid: Option<u32>,
    /// Manifests by branch. A depot served from another app
    /// (`depotfromapp`) has none.
    pub manifests: BTreeMap<String, ManifestRef>,
}

impl DepotInfo {
    fn is_windows(&self) -> bool {
        self.oslist.is_empty() || self.oslist.iter().any(|os| os == WINDOWS)
    }
}

/// What decides which of an app's Windows depots to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepotFilter {
    pub language: String,
    pub owned_appids: BTreeSet<u32>,
}

impl Default for DepotFilter {
    fn default() -> Self {
        Self {
            language: "english".to_owned(),
            owned_appids: BTreeSet::new(),
        }
    }
}

/// A depot chosen for a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub size: u64,
}

/// One owned app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedApp {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    /// Bytes of the Windows depots [`windows_depots`](Self::windows_depots)
    /// picks on the default branch.
    pub size: u64,
    pub default_branch: String,
    pub depots: Vec<DepotInfo>,
    /// Always [`DRM_UNKNOWN`] here.
    pub drm: String,
    /// Every platform the app or any depot lists, sorted.
    pub platforms: Vec<String>,
    /// `depots/branches/<branch>/buildid`.
    pub builds: BTreeMap<String, u64>,
    pub change_number: u32,
    app_oslist: Vec<String>,
}

impl OwnedApp {
    /// Parses a PICS product-info buffer. `None` when there is no buffer or
    /// it is not `KeyValues`.
    #[must_use]
    pub fn parse(info: &AppInfo, filter: &DepotFilter) -> Option<Self> {
        let root = KeyValue::from_text(info.text.as_deref()?).ok()?;
        let app_oslist = oslist(text(&root, &["common", "oslist"]));
        let mut depots = Vec::new();
        let mut builds = BTreeMap::new();
        let mut default_branch = DEFAULT_BRANCH.to_owned();
        if let Some(section) = children(root.get("depots")) {
            for (key, entry) in section {
                if let Ok(depot_id) = key.parse() {
                    depots.push(parse_depot(depot_id, entry));
                }
            }
            if let Some(branches) = children(root.get("depots").and_then(|d| d.get("branches"))) {
                for (branch, entry) in branches {
                    if let Some(build) = text(entry, &["buildid"]).and_then(|id| id.parse().ok()) {
                        builds.insert(branch.clone(), build);
                    }
                }
                if !branches.contains_key(DEFAULT_BRANCH)
                    && let Some(first) = branches.keys().next()
                {
                    default_branch.clone_from(first);
                }
            }
        }
        let mut platforms: BTreeSet<String> = app_oslist.iter().cloned().collect();
        platforms.extend(depots.iter().flat_map(|depot| depot.oslist.iter().cloned()));
        let mut app = Self {
            appid: info.appid,
            name: text(&root, &["common", "name"])
                .unwrap_or_default()
                .to_owned(),
            installdir: text(&root, &["config", "installdir"])
                .unwrap_or_default()
                .to_owned(),
            size: 0,
            default_branch,
            depots,
            drm: DRM_UNKNOWN.to_owned(),
            platforms: platforms.into_iter().collect(),
            builds,
            change_number: info.change_number,
            app_oslist,
        };
        app.size = app
            .windows_depots(&app.default_branch, filter)
            .map_or(0, |depots| depots.iter().map(|depot| depot.size).sum());
        Some(app)
    }

    /// True when the app ships at least one depot that runs on Windows and
    /// has a manifest of its own.
    #[must_use]
    pub fn has_windows_depot(&self) -> bool {
        self.runs_on_windows()
            && self
                .depots
                .iter()
                .any(|depot| depot.is_windows() && !depot.manifests.is_empty())
    }

    fn runs_on_windows(&self) -> bool {
        self.app_oslist.is_empty() || self.app_oslist.iter().any(|os| os == WINDOWS)
    }

    /// The depots to fetch for `branch`: Windows, x86-64 plus architecture
    /// neutral, or 32-bit when the app has no x86-64 depot; the filter's
    /// language or none; no low-violence variants; DLC depots only when the
    /// DLC is owned. A depot with no manifest on the branch is left out,
    /// which covers shared redistributables served from another app.
    ///
    /// # Errors
    ///
    /// [`Error::NoWindowsDepot`] naming the platforms the app has, and
    /// [`Error::NoManifestForBranch`] when no eligible depot is on `branch`.
    pub fn windows_depots(
        &self,
        branch: &str,
        filter: &DepotFilter,
    ) -> Result<Vec<PlannedDepot>, Error> {
        if !self.has_windows_depot() {
            return Err(Error::NoWindowsDepot {
                appid: self.appid,
                platforms: self.platforms.clone(),
            });
        }
        let eligible: Vec<&DepotInfo> = self
            .depots
            .iter()
            .filter(|depot| depot.is_windows() && depot.manifests.contains_key(branch))
            .filter(|depot| {
                depot
                    .language
                    .as_ref()
                    .is_none_or(|language| *language == filter.language)
            })
            .filter(|depot| !depot.low_violence)
            .filter(|depot| {
                depot
                    .dlc_appid
                    .is_none_or(|appid| filter.owned_appids.contains(&appid))
            })
            .collect();
        let has_x86_64 = eligible.iter().any(|depot| depot.osarch == Some(X86_64));
        let planned: Vec<PlannedDepot> = eligible
            .into_iter()
            .filter(|depot| !has_x86_64 || depot.osarch.is_none_or(|arch| arch == X86_64))
            .filter_map(|depot| {
                let manifest = depot.manifests.get(branch)?;
                Some(PlannedDepot {
                    depot_id: depot.depot_id,
                    manifest_id: manifest.manifest_id,
                    size: manifest.size,
                })
            })
            .collect();
        if planned.is_empty() {
            return Err(Error::NoManifestForBranch {
                appid: self.appid,
                branch: branch.to_owned(),
            });
        }
        Ok(planned)
    }
}

/// The owned apps of one account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Every owned app PICS described, Windows or not.
    pub apps: Vec<OwnedApp>,
    /// What the licenses grant, including DLC.
    pub owned_appids: BTreeSet<u32>,
    /// Owned apps PICS answered for without usable info.
    pub skipped: Vec<u32>,
}

impl Library {
    /// Reads the licenses, their packages, and every granted app's product
    /// info from a logged-in session.
    ///
    /// # Errors
    ///
    /// [`Error::Auth`] from the session.
    pub async fn fetch<S: SteamSession>(session: &S) -> Result<Self, Error> {
        let licenses = session.license_list().await?;
        let appids = session.package_apps(&licenses).await?;
        let infos = session.app_info(&appids).await?;
        Ok(Self::from_app_info(&appids, &infos))
    }

    /// Builds a library from answers already in hand.
    #[must_use]
    pub fn from_app_info(appids: &[u32], infos: &[AppInfo]) -> Self {
        let owned_appids: BTreeSet<u32> = appids.iter().copied().collect();
        let filter = DepotFilter {
            owned_appids: owned_appids.clone(),
            ..DepotFilter::default()
        };
        let apps: Vec<OwnedApp> = infos
            .iter()
            .filter_map(|info| OwnedApp::parse(info, &filter))
            .collect();
        let described: BTreeSet<u32> = apps.iter().map(|app| app.appid).collect();
        Self {
            skipped: owned_appids.difference(&described).copied().collect(),
            apps,
            owned_appids,
        }
    }

    /// The installable subset: apps with a Windows depot and an install
    /// directory.
    pub fn windows_apps(&self) -> impl Iterator<Item = &OwnedApp> {
        self.apps
            .iter()
            .filter(|app| app.has_windows_depot() && !app.installdir.is_empty())
    }

    /// One owned app, for installing.
    ///
    /// # Errors
    ///
    /// [`Error::AppNotOwned`] when the account does not own it,
    /// [`Error::NoWindowsDepot`] naming the platforms when it has no Windows
    /// depot.
    pub fn windows_app(&self, appid: u32) -> Result<&OwnedApp, Error> {
        let app = self
            .apps
            .iter()
            .find(|app| app.appid == appid)
            .ok_or(Error::AppNotOwned { appid })?;
        if !app.has_windows_depot() {
            return Err(Error::NoWindowsDepot {
                appid,
                platforms: app.platforms.clone(),
            });
        }
        Ok(app)
    }

    /// The filter that matches this account: English, and DLC it owns.
    #[must_use]
    pub fn filter(&self) -> DepotFilter {
        DepotFilter {
            owned_appids: self.owned_appids.clone(),
            ..DepotFilter::default()
        }
    }
}

fn children(node: Option<&KeyValue>) -> Option<&BTreeMap<String, KeyValue>> {
    match &node?.value {
        KvValue::Children(children) => Some(children),
        _ => None,
    }
}

fn text<'a>(root: &'a KeyValue, path: &[&str]) -> Option<&'a str> {
    let mut node = root;
    for key in path {
        node = node.get(key)?;
    }
    node.as_str()
}

fn oslist(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(|os| os.trim().to_ascii_lowercase())
        .filter(|os| !os.is_empty())
        .collect()
}

fn parse_depot(depot_id: u32, entry: &KeyValue) -> DepotInfo {
    let mut manifests = BTreeMap::new();
    if let Some(section) = children(entry.get("manifests")) {
        for (branch, value) in section {
            // Older buffers hold the gid directly; newer ones a map.
            let manifest_id = value.as_str().or_else(|| text(value, &["gid"]));
            if let Some(manifest_id) = manifest_id.and_then(|id| id.parse().ok()) {
                manifests.insert(
                    branch.clone(),
                    ManifestRef {
                        manifest_id,
                        size: text(value, &["size"])
                            .or_else(|| text(entry, &["maxsize"]))
                            .and_then(|size| size.parse().ok())
                            .unwrap_or(0),
                    },
                );
            }
        }
    }
    DepotInfo {
        depot_id,
        oslist: oslist(text(entry, &["config", "oslist"])),
        osarch: text(entry, &["config", "osarch"]).and_then(|arch| arch.parse().ok()),
        language: text(entry, &["config", "language"])
            .filter(|language| !language.is_empty())
            .map(str::to_owned),
        low_violence: text(entry, &["config", "lowviolence"]) == Some("1"),
        dlc_appid: text(entry, &["dlcappid"]).and_then(|appid| appid.parse().ok()),
        manifests,
    }
}
