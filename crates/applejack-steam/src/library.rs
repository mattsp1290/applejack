//! The owned-app library: licenses to packages to appids to PICS product
//! info, reduced to plain values. Persisting them (the `apps` table) belongs
//! to the caller, since this crate does not depend on `applejack-config`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use steamroom::types::key_value::{KeyValue, KvValue};

use crate::Error;
use crate::session::{AppInfo, PackageGrants, SteamSession};

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
    /// `depotfromapp`: the app that holds this depot.
    pub depot_from_app: Option<u32>,
    /// Manifests by branch. A `depotfromapp` depot may have none of its own,
    /// and takes the parent app's when the library resolves it.
    pub manifests: BTreeMap<String, ManifestRef>,
}

impl DepotInfo {
    fn is_windows(&self) -> bool {
        self.oslist.is_empty() || self.oslist.iter().any(|os| os == WINDOWS)
    }

    /// A `depotfromapp` depot with no manifests yet.
    fn awaits_parent(&self) -> bool {
        self.depot_from_app.is_some() && self.manifests.is_empty()
    }

    /// The manifest for `branch`. A depot need not be on every branch, and a
    /// parent app's rarely is, so a branch the app has falls back to `public`
    /// as `DepotDownloader` does; the flag says so.
    fn manifest_for(&self, branch: &str, branch_exists: bool) -> Option<(ManifestRef, bool)> {
        if let Some(manifest) = self.manifests.get(branch) {
            return Some((*manifest, false));
        }
        let fallback = self
            .manifests
            .get(DEFAULT_BRANCH)
            .filter(|_| branch_exists)?;
        Some((*fallback, true))
    }
}

/// What decides which of an app's Windows depots to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepotFilter {
    pub language: String,
    pub owned_appids: BTreeSet<u32>,
    pub owned_depots: BTreeSet<u32>,
}

impl Default for DepotFilter {
    fn default() -> Self {
        Self {
            language: "english".to_owned(),
            owned_appids: BTreeSet::new(),
            owned_depots: BTreeSet::new(),
        }
    }
}

/// A depot chosen for a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub size: u64,
    /// The app to name for this depot's manifest and CDN token, when it is
    /// not the one being installed. The depot key still goes by that one.
    #[serde(default)]
    pub source_app: Option<u32>,
    /// The manifest is the parent app's `public`, whatever branch is planned.
    #[serde(default)]
    pub public_manifest: bool,
}

/// What [`OwnedApp::windows_depots`] chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepotSelection {
    pub depots: Vec<PlannedDepot>,
    /// Depots that would be fetched if the account licensed them.
    pub not_licensed: Vec<u32>,
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
    /// `common/freetodownload`: depots need no license.
    pub free_to_download: bool,
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
        let depots_node = child(&root, "depots");
        if let Some(section) = children(depots_node) {
            for (key, entry) in section {
                if let Ok(depot_id) = key.parse() {
                    depots.push(parse_depot(depot_id, entry));
                }
            }
            // The map's keys are text, so "1000" would sort before "999".
            depots.sort_by_key(|depot: &DepotInfo| depot.depot_id);
            if let Some(branches) = children(depots_node.and_then(|d| child(d, "branches"))) {
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
            free_to_download: text(&root, &["common", "freetodownload"])
                .and_then(|value| value.parse::<i32>().ok())
                .is_some_and(|value| value != 0),
            app_oslist,
        };
        app.measure(filter);
        Some(app)
    }

    fn measure(&mut self, filter: &DepotFilter) {
        self.size = self
            .windows_depots(&self.default_branch, filter)
            .map_or(0, |selection| {
                selection.depots.iter().map(|depot| depot.size).sum()
            });
    }

    /// The apps whose product info this one's shared depots need.
    fn parents(&self) -> impl Iterator<Item = u32> {
        self.depots
            .iter()
            .filter(|depot| depot.awaits_parent())
            .filter_map(|depot| depot.depot_from_app)
            .filter(|parent| *parent != self.appid)
    }

    /// Resolves `depotfromapp` depots one level, as `DepotDownloader` does.
    /// One the parent cannot supply stays unresolved, and
    /// [`windows_depots`](Self::windows_depots) refuses it.
    fn link_shared(&mut self, parents: &BTreeMap<u32, Self>, filter: &DepotFilter) {
        let appid = self.appid;
        for depot in &mut self.depots {
            let source = depot
                .depot_from_app
                .filter(|parent| depot.manifests.is_empty() && *parent != appid)
                .and_then(|parent| parents.get(&parent))
                .and_then(|parent| parent.depots.iter().find(|d| d.depot_id == depot.depot_id));
            if let Some(source) = source.filter(|source| !source.manifests.is_empty()) {
                depot.manifests.clone_from(&source.manifests);
            }
        }
        self.measure(filter);
    }

    /// True when the app ships at least one depot that runs on Windows and
    /// has a manifest, its own or a parent app's.
    #[must_use]
    pub fn has_windows_depot(&self) -> bool {
        self.runs_on_windows()
            && self.depots.iter().any(|depot| {
                depot.is_windows() && (!depot.manifests.is_empty() || depot.awaits_parent())
            })
    }

    /// `AccountHasAccess` in `DepotDownloader`: a package lists the depot, or
    /// the app needs no license. Owning a depot's DLC counts too, and it is
    /// what a free app's paid DLC still needs.
    fn is_licensed(&self, depot: &DepotInfo, filter: &DepotFilter) -> bool {
        let id = depot.depot_id;
        filter.owned_depots.contains(&id)
            || filter.owned_appids.contains(&id)
            || depot.dlc_appid.map_or(self.free_to_download, |dlc| {
                filter.owned_appids.contains(&dlc)
            })
    }

    fn runs_on_windows(&self) -> bool {
        self.app_oslist.is_empty() || self.app_oslist.iter().any(|os| os == WINDOWS)
    }

    /// The depots to fetch for `branch`: Windows, x86-64 plus architecture
    /// neutral, or 32-bit when the app has no x86-64 depot; the filter's
    /// language or none; no low-violence variants; only what the account
    /// licenses, with the rest reported. A depot with no manifest on a branch
    /// the app has takes its `public` one.
    ///
    /// # Errors
    ///
    /// [`Error::NoWindowsDepot`] naming the platforms the app has,
    /// [`Error::NoManifestForBranch`] when no eligible depot is on `branch`,
    /// [`Error::NoLicensedDepot`] when the account licenses none of them, and
    /// [`Error::UnresolvedSharedDepot`] for a licensed `depotfromapp` depot
    /// with no manifest, since installing without it would call a title
    /// complete with content missing.
    pub fn windows_depots(
        &self,
        branch: &str,
        filter: &DepotFilter,
    ) -> Result<DepotSelection, Error> {
        if !self.has_windows_depot() {
            return Err(Error::NoWindowsDepot {
                appid: self.appid,
                platforms: self.platforms.clone(),
            });
        }
        let branch_exists = self.builds.contains_key(branch);
        let (licensed, unlicensed): (Vec<&DepotInfo>, Vec<&DepotInfo>) = self
            .depots
            .iter()
            .filter(|depot| {
                depot.is_windows()
                    && (depot.manifest_for(branch, branch_exists).is_some()
                        || depot.awaits_parent())
            })
            .filter(|depot| {
                depot
                    .language
                    .as_ref()
                    .is_none_or(|language| *language == filter.language)
            })
            .filter(|depot| !depot.low_violence)
            .partition(|depot| self.is_licensed(depot, filter));
        // Over both lists: 32-bit is for an app that ships only that, not a
        // stand-in for an x86-64 depot this account lacks.
        let has_x86_64 = licensed
            .iter()
            .chain(&unlicensed)
            .any(|depot| depot.osarch == Some(X86_64));
        let fits =
            |depot: &&DepotInfo| !has_x86_64 || depot.osarch.is_none_or(|arch| arch == X86_64);
        let not_licensed: Vec<u32> = unlicensed
            .into_iter()
            .filter(fits)
            .map(|depot| depot.depot_id)
            .collect();
        let mut depots = Vec::new();
        for depot in licensed.into_iter().filter(fits) {
            depots.push(self.planned(depot, branch)?);
        }
        if !depots.is_empty() {
            return Ok(DepotSelection {
                depots,
                not_licensed,
            });
        }
        // A depot still waiting on its parent says nothing about the branch.
        let on_branch = branch_exists
            || self.depots.iter().any(|depot| {
                not_licensed.contains(&depot.depot_id)
                    && depot.manifest_for(branch, false).is_some()
            });
        Err(if on_branch && !not_licensed.is_empty() {
            Error::NoLicensedDepot {
                appid: self.appid,
                depots: not_licensed,
            }
        } else {
            Error::NoManifestForBranch {
                appid: self.appid,
                branch: branch.to_owned(),
            }
        })
    }

    fn planned(&self, depot: &DepotInfo, branch: &str) -> Result<PlannedDepot, Error> {
        let branch_exists = self.builds.contains_key(branch);
        let found = depot.manifest_for(branch, branch_exists);
        let (manifest, public_manifest) = found.ok_or(Error::UnresolvedSharedDepot {
            appid: self.appid,
            depot_id: depot.depot_id,
            from_app: depot.depot_from_app.unwrap_or(self.appid),
        })?;
        Ok(PlannedDepot {
            depot_id: depot.depot_id,
            manifest_id: manifest.manifest_id,
            size: manifest.size,
            // `DepotDownloader` names the launching app for a free title.
            source_app: depot.depot_from_app.filter(|_| !self.free_to_download),
            public_manifest,
        })
    }
}

/// One app's fresh product info, with its shared depots resolved. `None` when
/// Steam has no usable info for it.
pub(crate) async fn fetch_app<S: SteamSession>(
    session: &S,
    appid: u32,
    filter: &DepotFilter,
) -> Result<Option<OwnedApp>, Error> {
    let infos = session.app_info(&[appid]).await?;
    let Some(mut app) = infos
        .iter()
        .filter(|info| info.appid == appid)
        .find_map(|info| OwnedApp::parse(info, filter))
    else {
        return Ok(None);
    };
    let wanted: BTreeSet<u32> = app.parents().collect();
    if !wanted.is_empty() {
        let wanted: Vec<u32> = wanted.into_iter().collect();
        let infos = session.app_info(&wanted).await?;
        let parents = infos
            .iter()
            .filter_map(|info| OwnedApp::parse(info, filter))
            .map(|parent| (parent.appid, parent))
            .collect();
        app.link_shared(&parents, filter);
    }
    Ok(Some(app))
}

/// The owned apps of one account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Every owned app PICS described, Windows or not.
    pub apps: Vec<OwnedApp>,
    /// What the licenses grant, including DLC.
    pub owned_appids: BTreeSet<u32>,
    #[serde(default)]
    pub owned_depots: BTreeSet<u32>,
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
        let grants = session.package_grants(&licenses).await?;
        let appids: Vec<u32> = grants.appids.iter().copied().collect();
        let mut infos = session.app_info(&appids).await?;
        // Parents are often not owned: 228980 holds the redistributables.
        let library = Self::from_app_info(&grants, &infos);
        let described: BTreeSet<u32> = infos.iter().map(|info| info.appid).collect();
        let parents: BTreeSet<u32> = library
            .apps
            .iter()
            .flat_map(OwnedApp::parents)
            .filter(|parent| !described.contains(parent))
            .collect();
        if parents.is_empty() {
            return Ok(library);
        }
        let parents: Vec<u32> = parents.into_iter().collect();
        infos.extend(session.app_info(&parents).await?);
        Ok(Self::from_app_info(&grants, &infos))
    }

    /// Builds a library from answers already in hand. `infos` may hold apps
    /// the account does not own, which only serve to resolve `depotfromapp`.
    #[must_use]
    pub fn from_app_info(grants: &PackageGrants, infos: &[AppInfo]) -> Self {
        let filter = DepotFilter {
            owned_appids: grants.appids.clone(),
            owned_depots: grants.depot_ids.clone(),
            ..DepotFilter::default()
        };
        let parsed: BTreeMap<u32, OwnedApp> = infos
            .iter()
            .filter_map(|info| OwnedApp::parse(info, &filter))
            .map(|app| (app.appid, app))
            .collect();
        let mut apps: Vec<OwnedApp> = parsed
            .values()
            .filter(|app| grants.appids.contains(&app.appid))
            .cloned()
            .collect();
        for app in &mut apps {
            app.link_shared(&parsed, &filter);
        }
        let described: BTreeSet<u32> = apps.iter().map(|app| app.appid).collect();
        Self {
            skipped: grants.appids.difference(&described).copied().collect(),
            apps,
            owned_appids: grants.appids.clone(),
            owned_depots: grants.depot_ids.clone(),
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

    /// The filter that matches this account: English, and what it licenses.
    #[must_use]
    pub fn filter(&self) -> DepotFilter {
        DepotFilter {
            owned_appids: self.owned_appids.clone(),
            owned_depots: self.owned_depots.clone(),
            ..DepotFilter::default()
        }
    }
}

/// `KeyValue::get` with `SteamKit`'s indexer rule: an exact key, or any
/// spelling of it. Product info keys are not reliably one case, and an absent
/// `oslist` reads as "every platform".
fn child<'a>(node: &'a KeyValue, key: &str) -> Option<&'a KeyValue> {
    node.get(key).or_else(|| {
        children(Some(node))?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    })
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
        node = child(node, key)?;
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
    if let Some(section) = children(child(entry, "manifests")) {
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
        depot_from_app: text(entry, &["depotfromapp"]).and_then(|appid| appid.parse().ok()),
        manifests,
    }
}
