//! The `rosetta-wine` engine: x86-64 Wine under Rosetta 2. Rosetta runs the
//! x86-64 loader directly (no `arch -x86_64`), so the engine is a plain
//! process spawner over the bundle described by `BUNDLE.toml`.
//!
//! - [`prefix`]: the prefix builder and `apply_overrides`.
//! - [`launch`]: the environment rows, spawning, `Session` methods.
//! - [`process`]: discovery by session id, `wineserver` liveness.
//! - [`d3dmetal`]: the user-supplied overlay.
//! - [`snapshot`]: prefix snapshots, restore, reset, prune.
//! - [`cpuid`]: the Rosetta AVX probe.

use std::path::Path;

use crate::bundle;
use crate::{
    Arch, DoctorReport, Engine, EngineId, EngineLayout, Error, Finding, GraphicsBackend,
    HidrawMode, Host, InstallRequest, InstalledEngine, LaunchSpec, Prefix, PrefixSpec, Progress,
    Result, Session, Severity, SyncMode,
};

mod clock;
pub mod cpuid;
pub mod d3dmetal;
pub mod launch;
pub mod prefix;
pub mod process;
pub mod snapshot;

pub use cpuid::{CpuidFeatures, CpuidProbe};
pub use d3dmetal::D3DMetalOverlay;
pub use prefix::{PREFIX_STEPS, PrefixRecord, PrefixStep};
pub use process::Process;
pub use snapshot::Snapshot;

/// Now as `YYYY-MM-DDTHH:MM:SSZ`, the timestamp text the core records.
#[must_use]
pub fn utc_now() -> String {
    clock::Utc::now().rfc3339()
}

/// The engine. Stateless: everything it needs arrives as an
/// [`InstalledEngine`], a [`Prefix`], or a spec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RosettaWine;

impl RosettaWine {
    /// Runs the bundle's CPUID probe with and without
    /// `ROSETTA_ADVERTISE_AVX=1` and returns both parses for the core to
    /// record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidBundle`] when the bundle has no `[probes]`
    /// table, [`Error::Spawn`], or [`Error::ProbeOutput`].
    pub fn probe_cpuid(&self, installed: &InstalledEngine) -> Result<CpuidProbe> {
        cpuid::run(installed)
    }

    /// Reads back a prefix this engine made from its record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPrefix`] when the record is absent, does not
    /// parse, or names another engine.
    pub fn open_prefix(&self, dir: &Path) -> Result<Prefix> {
        let record = PrefixRecord::read(dir)?;
        if record.engine != self.id() {
            return Err(Error::InvalidPrefix {
                path: dir.to_path_buf(),
                reason: format!("it belongs to engine {}, not {}", record.engine, self.id()),
            });
        }
        Ok(record.to_prefix(dir))
    }
}

impl Engine for RosettaWine {
    fn id(&self) -> EngineId {
        EngineId::RosettaWine
    }

    fn doctor(&self, host: &Host, installed: Option<&InstalledEngine>) -> DoctorReport {
        let mut findings = Vec::new();
        let rosetta = match (host.arch, host.rosetta_present) {
            (Arch::X86_64, _) => (
                Severity::Info,
                "x86-64 host: the engine runs natively, Rosetta is not involved".to_owned(),
            ),
            (Arch::Arm64, Some(true)) => (Severity::Info, "Rosetta 2 is installed".to_owned()),
            (Arch::Arm64, Some(false)) => (Severity::Error, Error::RosettaMissing.to_string()),
            (Arch::Arm64, None) => (
                Severity::Warning,
                "could not tell whether Rosetta 2 is installed".to_owned(),
            ),
        };
        push(&mut findings, rosetta);
        match installed {
            Some(installed) => self.bundle_findings(host, installed, &mut findings),
            None => push(
                &mut findings,
                (
                    Severity::Error,
                    "engine rosetta-wine is not installed".to_owned(),
                ),
            ),
        }
        let runnable = findings
            .iter()
            .all(|finding| finding.severity != Severity::Error);
        DoctorReport { runnable, findings }
    }

    fn install(
        &self,
        host: &Host,
        layout: &EngineLayout,
        request: &InstallRequest,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<InstalledEngine> {
        if host.arch == Arch::Arm64 && host.rosetta_present == Some(false) {
            return Err(Error::RosettaMissing);
        }
        bundle::install(layout, request, progress)
    }

    fn create_prefix(
        &self,
        installed: &InstalledEngine,
        prefix_dir: &Path,
        spec: &PrefixSpec,
    ) -> Result<Prefix> {
        prefix::create(installed, prefix_dir, spec)
    }

    fn apply_overrides(
        &self,
        installed: &InstalledEngine,
        prefix: &Prefix,
        backend: GraphicsBackend,
        sync_mode: SyncMode,
        hidraw: HidrawMode,
    ) -> Result<Prefix> {
        prefix::apply(installed, prefix, backend, sync_mode, hidraw)
    }

    fn launch(
        &self,
        installed: &InstalledEngine,
        prefix: &Prefix,
        spec: &LaunchSpec,
    ) -> Result<Session> {
        launch::spawn(installed, prefix, spec)
    }
}

impl RosettaWine {
    fn bundle_findings(
        self,
        host: &Host,
        installed: &InstalledEngine,
        findings: &mut Vec<Finding>,
    ) {
        let bundle = &installed.bundle;
        let mut note = |severity, message: String| push(findings, (severity, message));
        note(
            Severity::Info,
            format!(
                "engine version {} (Wine {}) at `{}`",
                installed.version,
                bundle.wine.version,
                installed.root.display()
            ),
        );
        match (
            parse_macos(&host.macos_version),
            parse_macos(&bundle.min_macos),
        ) {
            (Some(have), Some(need)) if have < need => note(
                Severity::Error,
                format!(
                    "macOS {} is older than the bundle's minimum {}",
                    host.macos_version, bundle.min_macos
                ),
            ),
            (Some(have), _) if have < (15, 0) && !bundle.overrides.dxmt.is_empty() => note(
                Severity::Warning,
                "macOS 14: DXMT turns unsupported Metal 3.2 intrinsics into no-ops".to_owned(),
            ),
            (None, _) => note(
                Severity::Warning,
                format!("cannot parse macOS version `{}`", host.macos_version),
            ),
            _ => {}
        }
        for (what, path) in [
            ("loader", &bundle.wine.loader),
            ("wineserver", &bundle.wine.server),
        ] {
            if !installed.root.join(path).is_file() {
                note(
                    Severity::Error,
                    format!("{what} `{}` is missing", path.display()),
                );
            }
        }
        match &bundle.probes {
            Some(probes) => {
                for (what, path) in [
                    ("cpuid", &probes.cpuid),
                    ("dx11_sample", &probes.dx11_sample),
                    ("xinput_check", &probes.xinput_check),
                ] {
                    if !installed.root.join(path).is_file() {
                        note(
                            Severity::Warning,
                            format!("probe {what} `{}` is missing", path.display()),
                        );
                    }
                }
            }
            None => note(Severity::Warning, "bundle ships no probes".to_owned()),
        }
        let capabilities = self.capabilities(bundle);
        let backends: Vec<&str> = capabilities
            .backends
            .iter()
            .map(|backend| backend.as_str())
            .collect();
        let modes: Vec<&str> = capabilities
            .sync_modes
            .iter()
            .map(|mode| mode.as_str())
            .collect();
        note(
            Severity::Info,
            format!(
                "backends: {}; sync modes: {} (default {}); 32-bit: {}",
                backends.join(", "),
                modes.join(", "),
                modes.first().copied().unwrap_or("none"),
                if capabilities.supports_32bit {
                    "yes"
                } else {
                    "no"
                }
            ),
        );
        if bundle.d3dmetal.is_some() {
            note(
                Severity::Info,
                if d3dmetal::overlay_present(installed) {
                    "D3DMetal overlay present".to_owned()
                } else {
                    "D3DMetal overlay not imported".to_owned()
                },
            );
        }
    }
}

fn push(findings: &mut Vec<Finding>, (severity, message): (Severity, String)) {
    findings.push(Finding { severity, message });
}

fn parse_macos(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}
