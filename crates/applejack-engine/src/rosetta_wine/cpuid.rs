//! The CPUID probe: runs the bundle's `probes/cpuid` with and without
//! `ROSETTA_ADVERTISE_AVX=1` and records what Rosetta advertised each time.
//! Whether the variable changes the bits is assumption A3; a launch sets it
//! only when a recorded probe says it does.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{Error, InstalledEngine, Result};

use super::launch::{ADVERTISE_AVX, child_path};

/// One run of the probe, as the JSON line it prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each field is one CPUID bit, recorded verbatim"
)]
pub struct CpuidFeatures {
    /// CPUID.1:ECX.AVX.
    pub avx: bool,
    /// CPUID.7.0:EBX.AVX2.
    pub avx2: bool,
    /// CPUID.1:ECX.OSXSAVE.
    pub osxsave: bool,
    /// XCR0 bits 1 and 2 (SSE and AVX state enabled by the OS).
    pub xcr0_ymm: bool,
}

/// Both runs of the probe on one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuidProbe {
    /// Without `ROSETTA_ADVERTISE_AVX`.
    pub plain: CpuidFeatures,
    /// With `ROSETTA_ADVERTISE_AVX=1`.
    pub advertised: CpuidFeatures,
}

impl CpuidProbe {
    /// True when the variable changed any advertised bit, which is what a
    /// launch needs to know before setting it.
    #[must_use]
    pub fn changes_bits(&self) -> bool {
        self.plain != self.advertised
    }
}

/// Parses the probe's output: one JSON object on the first non-empty line.
///
/// # Errors
///
/// Returns [`Error::ProbeOutput`] when no line parses.
pub fn parse(probe: &Path, output: &str) -> Result<CpuidFeatures> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    serde_json::from_str(line).map_err(|error| Error::ProbeOutput {
        probe: probe.to_path_buf(),
        reason: error.to_string(),
        output: output.to_owned(),
    })
}

pub(crate) fn run(installed: &InstalledEngine) -> Result<CpuidProbe> {
    let Some(probes) = &installed.bundle.probes else {
        return Err(Error::InvalidBundle(
            "no [probes] table, so there is no cpuid probe to run".to_owned(),
        ));
    };
    let probe = installed.root.join(&probes.cpuid);
    let plain = run_once(installed, &probe, false)?;
    let advertised = run_once(installed, &probe, true)?;
    Ok(CpuidProbe { plain, advertised })
}

fn run_once(installed: &InstalledEngine, probe: &Path, advertise: bool) -> Result<CpuidFeatures> {
    let mut command = Command::new(probe);
    command.env_clear().env("PATH", child_path(installed));
    if advertise {
        command.env(ADVERTISE_AVX, "1");
    }
    let output = command.output().map_err(|error| Error::Spawn {
        program: probe.to_path_buf(),
        source: error,
    })?;
    if !output.status.success() {
        return Err(Error::ProbeOutput {
            probe: probe.to_path_buf(),
            reason: format!("exited with {}", output.status),
            output: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    parse(probe, &String::from_utf8_lossy(&output.stdout))
}
