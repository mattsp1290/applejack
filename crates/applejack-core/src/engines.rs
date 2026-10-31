//! Engine operations: list, install, use, doctor, the CPUID probe, and the
//! `D3DMetal` overlay. The CLI and the RPC handler both call these.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use applejack_engine::rosetta_wine::{self, CpuidProbe, d3dmetal};
use applejack_engine::{
    DoctorReport, Engine, EngineId, EngineLayout, Error as EngineError, Finding, InstallLock,
    InstalledEngine, LocalBundle, Progress, RosettaWine, Severity, Version, embedded_manifest,
    versions,
};
use serde::{Deserialize, Serialize};

use crate::config::ConfigDb;
use crate::error::CoreError;
use crate::host::HostFacts;
use crate::state::Core;

/// `meta` key holding the last [`CpuidProbe`], as JSON.
pub const META_CPUID_PROBE: &str = "cpuid_probe";

const ENGINE: EngineId = EngineId::RosettaWine;

/// One row of `engine.list`: a version on disk, in the manifest, or both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineEntry {
    pub engine_id: String,
    pub version: String,
    pub installed: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineListResult {
    pub engines: Vec<EngineEntry>,
}

/// The result of `engine.probe_cpuid`: both runs and what they mean for a
/// launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuidProbeResult {
    #[serde(flatten)]
    pub probe: CpuidProbe,
    pub changes_bits: bool,
}

/// The result of `engine.d3dmetal.import`, as recorded in `d3dmetal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3dmetalImportResult {
    pub engine_version: String,
    pub overlay_dir: PathBuf,
    pub imported_from: PathBuf,
    pub gptk_version: String,
    pub accepted_at: String,
}

/// What a finished install left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    pub id: EngineId,
    pub version: Version,
    pub root: PathBuf,
    pub active: bool,
}

/// An install that passed the refusals in [`Core::plan_install`]. It owns
/// what it needs, so it can run on a job thread.
#[derive(Debug, Clone)]
pub struct EngineInstall {
    id: EngineId,
    layout: EngineLayout,
    from: PathBuf,
}

/// One [`Progress`] as `job.progress` fields: phase, done bytes, total
/// bytes, message.
#[must_use]
pub fn describe_progress(progress: Progress) -> (&'static str, Option<u64>, Option<u64>, String) {
    match progress {
        Progress::Packing { written } => (
            "pack",
            Some(written),
            None,
            "packing the bundle directory".to_owned(),
        ),
        Progress::Downloading { received, total } => {
            ("download", Some(received), total, "downloading".to_owned())
        }
        Progress::Verifying => ("verify", None, None, "verifying SHA-256".to_owned()),
        Progress::Extracting => ("extract", None, None, "extracting".to_owned()),
        Progress::Validating => ("validate", None, None, "validating the bundle".to_owned()),
        _ => ("install", None, None, "installing".to_owned()),
    }
}

impl EngineInstall {
    /// Packs a directory if `from` is one, installs through
    /// `Engine::install`, records the version, and activates it when no
    /// version is active.
    ///
    /// # Errors
    ///
    /// Returns the first failure; a failed install leaves no `<version>/`.
    /// One killed partway leaves its packed archive until the next install.
    pub fn run(self, progress: &mut dyn FnMut(Progress)) -> Result<InstalledVersion, CoreError> {
        let lock = InstallLock::acquire(&self.layout, self.id)?;
        lock.sweep_packed(&self.from);
        let local = LocalBundle::prepare(&self.layout, self.id, &self.from, progress)?;
        let host = HostFacts::gather().engine_host();
        let installed = RosettaWine.install(&host, &self.layout, &local.request, progress)?;
        strip_quarantine(&installed.root);
        let db = ConfigDb::open(&self.layout.data_dir().join(crate::config::DB_FILE))?;
        let active = db.record_engine_version(
            self.id.as_str(),
            installed.version.as_str(),
            &rosetta_wine::utc_now(),
        )?;
        Ok(InstalledVersion {
            id: self.id,
            version: installed.version,
            root: installed.root,
            active,
        })
    }
}

/// Best effort: a bundle built on this Mac carries no quarantine, and a
/// downloaded one Gatekeeper would stop is reported by the first launch.
fn strip_quarantine(root: &Path) {
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Core {
    fn available_engine(engine_id: &str) -> Result<EngineId, CoreError> {
        let id: EngineId = engine_id.parse().map_err(CoreError::Engine)?;
        if id == ENGINE {
            Ok(id)
        } else {
            Err(CoreError::EngineUnavailable { id })
        }
    }

    fn manifest_versions(id: EngineId) -> Result<Vec<String>, CoreError> {
        Ok(embedded_manifest()?
            .engine
            .into_iter()
            .filter(|entry| entry.id == id.as_str())
            .map(|entry| entry.version)
            .collect())
    }

    /// Checks an install can start. `from` is a bundle directory or a
    /// `.tar.zst`; without it the manifest decides.
    ///
    /// # Errors
    ///
    /// [`CoreError::UnsignedRefused`] for `from` without the opt-in,
    /// [`CoreError::NoPublishedVersion`] on an empty manifest,
    /// [`CoreError::DownloadUnavailable`] for a pinned version.
    pub fn plan_install(
        &self,
        engine_id: &str,
        from: Option<&Path>,
    ) -> Result<EngineInstall, CoreError> {
        let id = Self::available_engine(engine_id)?;
        let Some(from) = from else {
            return Err(match Self::manifest_versions(id)?.pop() {
                Some(version) => CoreError::DownloadUnavailable { id, version },
                None => CoreError::NoPublishedVersion { id },
            });
        };
        if !self.allows_unsigned() {
            return Err(CoreError::UnsignedRefused);
        }
        Ok(EngineInstall {
            id,
            layout: self.layout().clone(),
            from: from.to_path_buf(),
        })
    }

    /// Installed and published versions, oldest first.
    ///
    /// # Errors
    ///
    /// Returns the directory, manifest, or database read failure.
    pub fn engine_list(&mut self) -> Result<EngineListResult, CoreError> {
        let id = ENGINE;
        let installed = versions::list(self.layout(), id)?;
        let active = self.reading(|db| Ok(db.active_engine_version(id.as_str())?))?;
        let mut all = installed.clone();
        for version in Self::manifest_versions(id)? {
            let version = Version::new(version)?;
            if !all.contains(&version) {
                all.push(version);
            }
        }
        all.sort();
        let engines = all
            .into_iter()
            .map(|version| EngineEntry {
                engine_id: id.as_str().to_owned(),
                installed: installed.contains(&version),
                active: active.as_deref() == Some(version.as_str()),
                version: version.to_string(),
            })
            .collect();
        Ok(EngineListResult { engines })
    }

    /// Makes an installed version the active one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::NotInstalled`] for a version not on disk.
    pub fn engine_use(&mut self, engine_id: &str, version: &str) -> Result<(), CoreError> {
        let id = Self::available_engine(engine_id)?;
        let version = Version::new(version)?;
        InstalledEngine::open(self.layout(), id, &version)?;
        self.db()?
            .activate_engine_version(id.as_str(), version.as_str())?;
        Ok(())
    }

    /// The active bundle, or `None` when none is selected or it left the
    /// disk.
    pub(crate) fn active_engine(&mut self) -> Result<Option<InstalledEngine>, CoreError> {
        let id = ENGINE;
        let Some(version) = self.reading(|db| Ok(db.active_engine_version(id.as_str())?))? else {
            return Ok(None);
        };
        match InstalledEngine::open(self.layout(), id, &Version::new(version)?) {
            Ok(installed) => Ok(Some(installed)),
            Err(EngineError::NotInstalled { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn require_active_engine(&mut self) -> Result<InstalledEngine, CoreError> {
        match self.active_engine()? {
            Some(installed) => Ok(installed),
            None => Err(CoreError::NoActiveEngine {
                fix: self.install_fix()?,
            }),
        }
    }

    /// What to run when no engine is active.
    fn install_fix(&self) -> Result<String, CoreError> {
        let id = ENGINE;
        if let Some(version) = versions::list(self.layout(), id)?.pop() {
            return Ok(format!("run `applejack engine use {id} {version}`"));
        }
        Ok(match Self::manifest_versions(id)?.pop() {
            Some(_) => format!("run `applejack engine install {id}`"),
            None => crate::error::no_bundle_fix(id),
        })
    }

    /// Host and bundle readiness. Read-only and network-free. Having no
    /// engine, or one that does not open, is a finding with the fix, not a
    /// failure.
    ///
    /// # Errors
    ///
    /// Only when the fix itself cannot be worked out (an unreadable engine
    /// directory).
    pub fn engine_doctor(&mut self) -> Result<DoctorReport, CoreError> {
        let host = HostFacts::gather().engine_host();
        let (installed, opened) = match self.active_engine() {
            Ok(installed) => (installed, None),
            Err(error) => (None, Some(error)),
        };
        let mut report = self.engine.doctor(&host, installed.as_ref());
        if let Some(error) = opened {
            report.findings.push(Finding {
                severity: Severity::Error,
                message: error.to_string(),
            });
        }
        if installed.is_none() {
            report.runnable = false;
            report.findings.push(Finding {
                severity: Severity::Info,
                message: format!("fix: {}", self.install_fix()?),
            });
        }
        Ok(report)
    }

    /// Runs the active bundle's CPUID probe and stores both results.
    ///
    /// # Errors
    ///
    /// [`CoreError::NoActiveEngine`], or the probe or database failure.
    pub fn engine_probe_cpuid(&mut self) -> Result<CpuidProbeResult, CoreError> {
        let installed = self.require_active_engine()?;
        let probe = self.engine.probe_cpuid(&installed)?;
        let json = serde_json::to_string(&probe)?;
        self.db()?.meta_set(META_CPUID_PROBE, &json)?;
        Ok(CpuidProbeResult {
            probe,
            changes_bits: probe.changes_bits(),
        })
    }

    /// Copies `D3DMetal` from a mounted toolkit volume into the active
    /// engine's overlay and records the acceptance.
    ///
    /// # Errors
    ///
    /// [`CoreError::NoActiveEngine`], the engine's import refusals (the
    /// license among them), or the database failure.
    pub fn engine_d3dmetal_import(
        &mut self,
        path: &Path,
        accept_apple_license: bool,
    ) -> Result<D3dmetalImportResult, CoreError> {
        let installed = self.require_active_engine()?;
        let overlay = d3dmetal::import(&installed, path, accept_apple_license)?;
        self.db()?.d3dmetal_accept(
            installed.version.as_str(),
            &overlay.imported_from.to_string_lossy(),
            &overlay.gptk_version,
            &overlay.accepted_at,
        )?;
        Ok(D3dmetalImportResult {
            engine_version: installed.version.to_string(),
            overlay_dir: overlay.overlay_dir,
            imported_from: overlay.imported_from,
            gptk_version: overlay.gptk_version,
            accepted_at: overlay.accepted_at,
        })
    }

    /// Deletes the active engine's overlay and clears the record.
    ///
    /// # Errors
    ///
    /// [`CoreError::NoActiveEngine`], or the removal or database failure.
    pub fn engine_d3dmetal_remove(&mut self) -> Result<(), CoreError> {
        let installed = self.require_active_engine()?;
        d3dmetal::remove(&installed)?;
        self.db()?.d3dmetal_forget(installed.version.as_str())?;
        Ok(())
    }
}
