//! `BUNDLE.toml` writer. Every field and rule mirrors the engine crate's
//! reader (`applejack_engine::bundle::BundleToml`), so a spec this module
//! renders is a file the installer parses. The reader's own test parses
//! `xtask/tests/fixtures/BUNDLE.toml`; the test
//! `the_writer_reproduces_the_committed_bundle_fixture` keeps that fixture
//! equal to this writer's output for `build/bundle-spec.toml`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Everything `BUNDLE.toml` records about one engine payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSpec {
    pub id: String,
    pub version: String,
    pub arch: String,
    pub min_macos: String,
    pub wine: Wine,
    /// Ordered; the first entry is the default.
    pub sync_modes: Vec<String>,
    pub components: Components,
    #[serde(default)]
    pub overrides: Overrides,
    /// Present only when the bundle offers the `D3DMetal` backend (gate G4d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub d3dmetal: Option<D3DMetal>,
    pub probes: Probes,
    pub registry: Registry,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entitlements: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notarization_submission_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wine {
    pub version: String,
    pub source_url: String,
    pub source_rev: String,
    pub loader: String,
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preloader: Option<String>,
    pub wow64: Wow64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wow64 {
    #[serde(rename = "new")]
    New,
    #[serde(rename = "32on64")]
    ThirtyTwoOn64,
    #[serde(rename = "none")]
    None,
}

/// The named rows are the pinned components; every other row is an x86-64
/// Homebrew library family the dylib closure copies into `wine/lib/`, keyed
/// by formula name. The flattened map takes every unknown key, which is why
/// this struct alone carries no `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Components {
    pub dxmt: Component,
    pub vkd3d_proton: Component,
    pub moltenvk: Component,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kosmickrisp: Option<Component>,
    pub gstreamer: Component,
    /// Host-arch Authenticode tool under `probes/`; GPL-3.0, hence listed.
    pub osslsigncode: Component,
    /// `[components.<formula>]` for each Homebrew library family
    /// `cargo xtask engine-payload --close` copies in; the closure refuses
    /// to copy a file whose Cellar formula and version have no row here.
    #[serde(flatten)]
    pub libraries: BTreeMap<String, Component>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub version: String,
    pub license: String,
}

/// The `[overrides]` tables, one per backend that needs DLL overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "DllOverrides::is_empty")]
    pub dxmt: DllOverrides,
    #[serde(default, skip_serializing_if = "DllOverrides::is_empty")]
    pub vkd3d: DllOverrides,
    #[serde(default, skip_serializing_if = "DllOverrides::is_empty")]
    pub d3dmetal: DllOverrides,
}

/// Bundle-relative `.dll` paths per Windows architecture, the same shape as
/// the reader's `DllOverrides`: the installer copies each file into the
/// prefix and derives the `WINEDLLOVERRIDES` name from its stem, so stems are
/// unique per architecture (ignoring case) and `i386` is empty when
/// `wine.wow64 = "none"`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DllOverrides {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub x86_64: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub i386: Vec<String>,
}

impl DllOverrides {
    pub fn is_empty(&self) -> bool {
        self.x86_64.is_empty() && self.i386.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct D3DMetal {
    /// Relative to the `<version>.user/` overlay root.
    pub overlay_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probes {
    pub cpuid: String,
    pub dx11_sample: String,
    pub xinput_check: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The `winebus.sys` value that disables hidraw, as a `key\value` path.
    pub hidraw_key: String,
}

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Serialize(toml::ser::Error),
    Deserialize(toml::de::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "bundle spec: {message}"),
            Self::Serialize(error) => write!(f, "bundle spec: {error}"),
            Self::Deserialize(error) => write!(f, "bundle spec: {error}"),
        }
    }
}

impl std::error::Error for Error {}

const SYNC_MODES: [&str; 3] = ["msync", "esync", "none"];
/// The reader's `EngineId` set, each with the `arch` its bundles must declare.
const ENGINE_IDS: [(&str, &str); 2] = [("rosetta-wine", "x86_64"), ("native-arm64", "arm64")];
/// The reader's `EngineLayout` suffixes, which a version may not end with
/// because `engines/<id>/<version><suffix>/` are its sibling directories.
const RESERVED_VERSION_SUFFIXES: [&str; 3] = [".partial", ".user", ".download"];
/// Wine's license is not a `[wine]` field (the reader's table has none), so
/// the notices file names it here.
const WINE_LICENSE: &str = "LGPL-2.1-or-later";

impl BundleSpec {
    /// Parses a spec file written in the same shape as the output.
    ///
    /// # Errors
    ///
    /// Returns the TOML error or the first validation failure.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let spec: Self = toml::from_str(text).map_err(Error::Deserialize)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Renders the spec as `BUNDLE.toml` text.
    ///
    /// # Errors
    ///
    /// Returns the first validation failure or the serializer error.
    pub fn to_toml(&self) -> Result<String, Error> {
        self.validate()?;
        toml::to_string(self).map_err(Error::Serialize)
    }

    /// Renders `THIRD_PARTY_NOTICES.md` from the `[wine]` and `[components]`
    /// tables: one row per shipped third-party part with its version, SPDX
    /// license, and location in the bundle.
    ///
    /// # Errors
    ///
    /// Returns the first validation failure.
    pub fn to_notices(&self) -> Result<String, Error> {
        self.validate()?;
        let mut out = String::new();
        let _ = writeln!(out, "# Third-party notices");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Components of the `{}` engine bundle, version {}, with their versions and\n\
             SPDX license identifiers. Generated by `cargo xtask engine-payload\n\
             --write-bundle-toml` from the `[wine]` and `[components]` tables of the\n\
             bundle spec; edit `build/bundle-spec.toml`, not this file.",
            self.id, self.version
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "| Component | Version | License | Location in the bundle |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- |");
        let _ = writeln!(
            out,
            "| Wine | {} | {WINE_LICENSE} | `wine/` (built from {} at `{}`) |",
            self.wine.version, self.wine.source_url, self.wine.source_rev
        );
        for (name, component, location) in self.components.rows() {
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {location} |",
                component.version, component.license
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "The GStreamer row covers only the sub-packages `build/gstreamer/fetch.sh` stages\n\
             (the LGPL, MIT, BSD, and Apache set); the GPL and patent-restricted sub-packages\n\
             of the upstream package are not shipped. `osslsigncode` is a host-arch tool the\n\
             launcher runs to verify Authenticode signatures; it is not linked into Wine.\n\
             The `wine/lib/` rows are the x86-64 Homebrew libraries Wine links or loads by\n\
             name, copied in by `cargo xtask engine-payload --close` with the formula version\n\
             the build host had installed; each license is the shipped library's own, not\n\
             that of the formula's command-line tools."
        );
        Ok(out)
    }

    fn validate(&self) -> Result<(), Error> {
        let required = [
            ("id", &self.id),
            ("version", &self.version),
            ("arch", &self.arch),
            ("min_macos", &self.min_macos),
            ("wine.version", &self.wine.version),
            ("wine.source_url", &self.wine.source_url),
            ("wine.source_rev", &self.wine.source_rev),
            ("wine.loader", &self.wine.loader),
            ("wine.server", &self.wine.server),
            ("probes.cpuid", &self.probes.cpuid),
            ("probes.dx11_sample", &self.probes.dx11_sample),
            ("probes.xinput_check", &self.probes.xinput_check),
            ("registry.hidraw_key", &self.registry.hidraw_key),
        ];
        for (name, value) in required {
            if value.is_empty() {
                return Err(Error::Invalid(format!("`{name}` is empty")));
            }
        }
        let Some((_, arch)) = ENGINE_IDS.iter().find(|(id, _)| *id == self.id) else {
            return Err(Error::Invalid(format!(
                "`id` must be one of rosetta-wine, native-arm64 (got `{}`)",
                self.id
            )));
        };
        if self.arch != *arch {
            return Err(Error::Invalid(format!(
                "`arch` must be `{arch}` for engine `{}` (got `{}`)",
                self.id, self.arch
            )));
        }
        validate_version(&self.version)?;
        self.components.validate()?;
        if self.sync_modes.is_empty() {
            return Err(Error::Invalid("`sync_modes` is empty".into()));
        }
        let mut seen = BTreeSet::new();
        for mode in &self.sync_modes {
            if !SYNC_MODES.contains(&mode.as_str()) {
                return Err(Error::Invalid(format!("unknown sync mode `{mode}`")));
            }
            if !seen.insert(mode.as_str()) {
                return Err(Error::Invalid(format!(
                    "`sync_modes` lists `{mode}` more than once"
                )));
            }
        }
        if !self.wine.source_url.starts_with("https://") {
            return Err(Error::Invalid("`wine.source_url` must be https".into()));
        }
        let paths = [
            ("wine.loader", Some(&self.wine.loader)),
            ("wine.server", Some(&self.wine.server)),
            ("wine.preloader", self.wine.preloader.as_ref()),
            (
                "d3dmetal.overlay_dir",
                self.d3dmetal.as_ref().map(|d3dmetal| &d3dmetal.overlay_dir),
            ),
            ("probes.cpuid", Some(&self.probes.cpuid)),
            ("probes.dx11_sample", Some(&self.probes.dx11_sample)),
            ("probes.xinput_check", Some(&self.probes.xinput_check)),
        ];
        for (name, value) in paths {
            if let Some(value) = value {
                relative_path(name, value)?;
            }
        }
        for executable in self.entitlements.keys() {
            relative_path("entitlements", executable)?;
        }
        if !self.overrides.d3dmetal.is_empty() && self.d3dmetal.is_none() {
            return Err(Error::Invalid(
                "`overrides.d3dmetal` lists DLLs but there is no `[d3dmetal]` table".into(),
            ));
        }
        for (table, overrides) in [
            ("dxmt", &self.overrides.dxmt),
            ("vkd3d", &self.overrides.vkd3d),
            ("d3dmetal", &self.overrides.d3dmetal),
        ] {
            if self.wine.wow64 == Wow64::None && !overrides.i386.is_empty() {
                return Err(Error::Invalid(format!(
                    "`overrides.{table}.i386` must be empty when `wine.wow64` is \"none\""
                )));
            }
            validate_dlls(&format!("overrides.{table}.x86_64"), &overrides.x86_64)?;
            validate_dlls(&format!("overrides.{table}.i386"), &overrides.i386)?;
        }
        Ok(())
    }
}

impl Components {
    /// Every shipped component in notices order, with its display name and
    /// where it lands in the bundle.
    fn rows(&self) -> Vec<(String, &Component, String)> {
        let mut rows = vec![
            (
                "DXMT",
                &self.dxmt,
                "`dxmt/` and `wine/lib/wine/x86_64-unix/winemetal.so`",
            ),
            ("vkd3d-proton", &self.vkd3d_proton, "`vkd3d/`"),
            ("MoltenVK", &self.moltenvk, "`vulkan/`"),
        ];
        if let Some(kosmickrisp) = &self.kosmickrisp {
            rows.push(("KosmicKrisp", kosmickrisp, "`vulkan/`"));
        }
        rows.push((
            "GStreamer",
            &self.gstreamer,
            "`wine/lib/GStreamer.framework`",
        ));
        rows.push(("osslsigncode", &self.osslsigncode, "`probes/osslsigncode`"));
        let mut rows: Vec<(String, &Component, String)> = rows
            .into_iter()
            .map(|(name, component, location)| (name.to_owned(), component, location.to_owned()))
            .collect();
        rows.extend(self.libraries.iter().map(|(formula, component)| {
            (
                formula.clone(),
                component,
                format!(
                    "`wine/lib/` (x86-64 Homebrew formula `{formula}`, copied by the dylib closure)"
                ),
            )
        }));
        rows
    }

    /// Library rows are keyed by Homebrew formula name, which TOML writes as
    /// a bare key when it is `[A-Za-z0-9_-]+`.
    fn validate(&self) -> Result<(), Error> {
        for (formula, component) in &self.libraries {
            if formula.is_empty()
                || !formula
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(Error::Invalid(format!(
                    "`components.{formula}` is not a Homebrew formula name (letters, digits, `_`, `-`)"
                )));
            }
            for (field, value) in [
                ("version", &component.version),
                ("license", &component.license),
            ] {
                if value.is_empty() {
                    return Err(Error::Invalid(format!(
                        "`components.{formula}.{field}` is empty"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// The reader's `Version::new` rules: lowercase ASCII `[0-9a-z._+-]`, a
/// leading digit, no trailing `.`, and no layout suffix.
fn validate_version(version: &str) -> Result<(), Error> {
    let reason = if !version
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'.' | b'_' | b'+' | b'-'))
    {
        Some("must use only lowercase ASCII letters, digits, `.`, `_`, `+`, `-`")
    } else if !version.as_bytes()[0].is_ascii_digit() {
        Some("must start with a digit")
    } else if version.ends_with('.') {
        Some("ends with `.`")
    } else if RESERVED_VERSION_SUFFIXES
        .iter()
        .any(|suffix| version.ends_with(suffix))
    {
        Some("ends with a reserved layout suffix (.partial, .user, .download)")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(Error::Invalid(format!("`version` `{version}` {reason}"))),
        None => Ok(()),
    }
}

fn validate_dlls(name: &str, dlls: &[String]) -> Result<(), Error> {
    let mut stems = BTreeSet::new();
    for dll in dlls {
        relative_path(name, dll)?;
        let file = dll.rsplit('/').next().unwrap_or(dll);
        let (stem, extension) = file.rsplit_once('.').unwrap_or((file, ""));
        if stem.is_empty() || !extension.eq_ignore_ascii_case("dll") {
            return Err(Error::Invalid(format!(
                "`{name}` entry `{dll}` must have a `.dll` extension"
            )));
        }
        if !stems.insert(stem.to_ascii_lowercase()) {
            return Err(Error::Invalid(format!(
                "`{name}` entry `{dll}` repeats a DLL name already listed"
            )));
        }
    }
    Ok(())
}

/// A bundle path must stay inside the payload (or the overlay root): relative,
/// with no empty or `..` component. The same rule `pins.rs` applies to `dest`.
fn relative_path(name: &str, value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.starts_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::Invalid(format!(
            "`{name}` must be a relative path without `.` or `..` (got `{value}`)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use super::{
        BundleSpec, Component, Components, D3DMetal, DllOverrides, Overrides, Probes, Registry,
        Wine, Wow64,
    };

    fn component(version: &str, license: &str) -> Component {
        Component {
            version: version.into(),
            license: license.into(),
        }
    }

    fn spec() -> BundleSpec {
        BundleSpec {
            id: "rosetta-wine".into(),
            version: "11.0-1".into(),
            arch: "x86_64".into(),
            min_macos: "14.0".into(),
            wine: Wine {
                version: "11.0".into(),
                source_url: "https://dl.winehq.org/wine/source/11.0/wine-11.0.tar.xz".into(),
                source_rev: "wine-11.0".into(),
                loader: "wine/bin/wine".into(),
                server: "wine/bin/wineserver".into(),
                preloader: None,
                wow64: Wow64::New,
            },
            sync_modes: vec!["esync".into(), "none".into()],
            components: Components {
                dxmt: component("0.80", "MIT"),
                vkd3d_proton: component("3.1", "LGPL-2.1-or-later"),
                moltenvk: component("1.4.2", "Apache-2.0"),
                kosmickrisp: None,
                gstreamer: component("1.28.8", "LGPL-2.1-or-later"),
                osslsigncode: component("2.14", "GPL-3.0-or-later"),
                libraries: BTreeMap::new(),
            },
            overrides: Overrides {
                dxmt: DllOverrides {
                    x86_64: vec![
                        "dxmt/x86_64-windows/d3d11.dll".into(),
                        "dxmt/x86_64-windows/dxgi.dll".into(),
                        "dxmt/x86_64-windows/d3d10core.dll".into(),
                    ],
                    i386: vec![
                        "dxmt/i386-windows/d3d11.dll".into(),
                        "dxmt/i386-windows/dxgi.dll".into(),
                        "dxmt/i386-windows/d3d10core.dll".into(),
                    ],
                },
                vkd3d: DllOverrides {
                    x86_64: vec![
                        "vkd3d/x64/d3d12.dll".into(),
                        "vkd3d/x64/d3d12core.dll".into(),
                    ],
                    i386: vec![
                        "vkd3d/x86/d3d12.dll".into(),
                        "vkd3d/x86/d3d12core.dll".into(),
                    ],
                },
                d3dmetal: DllOverrides::default(),
            },
            d3dmetal: Some(D3DMetal {
                overlay_dir: "wine/lib/external".into(),
            }),
            probes: Probes {
                cpuid: "probes/cpuid".into(),
                dx11_sample: "probes/dx11-sample.exe".into(),
                xinput_check: "probes/xinput-check.exe".into(),
            },
            registry: Registry {
                hidraw_key: r"HKLM\System\CurrentControlSet\Services\WineBus\DisableHidraw".into(),
            },
            entitlements: BTreeMap::new(),
            notarization_submission_id: None,
        }
    }

    #[test]
    fn output_reparses_with_every_required_key() {
        let text = spec().to_toml().expect("serializes");
        let table: toml::Table = toml::from_str(&text).expect("valid TOML");
        for key in ["id", "version", "arch", "min_macos", "sync_modes"] {
            assert!(table.contains_key(key), "missing `{key}` in:\n{text}");
        }
        let wine = table["wine"].as_table().expect("wine table");
        for key in [
            "version",
            "source_url",
            "source_rev",
            "loader",
            "server",
            "wow64",
        ] {
            assert!(wine.contains_key(key), "missing `wine.{key}`");
        }
        assert_eq!(wine["wow64"].as_str(), Some("new"));
        assert!(!wine.contains_key("preloader"));
        let components = table["components"].as_table().expect("components");
        for key in [
            "dxmt",
            "vkd3d_proton",
            "moltenvk",
            "gstreamer",
            "osslsigncode",
        ] {
            let component = components[key].as_table().expect("component table");
            assert!(component.contains_key("version") && component.contains_key("license"));
        }
        assert!(!components.contains_key("kosmickrisp"));
        let overrides = table["overrides"].as_table().expect("overrides");
        for key in ["dxmt", "vkd3d"] {
            let per_arch = overrides[key].as_table().expect("per-arch table");
            assert!(per_arch["x86_64"].is_array(), "overrides.{key}.x86_64");
            assert!(per_arch["i386"].is_array(), "overrides.{key}.i386");
            assert!(
                !per_arch.contains_key("dlls"),
                "overrides.{key}.dlls is the old shape"
            );
        }
        assert!(
            !overrides.contains_key("d3dmetal"),
            "empty override tables are omitted"
        );
        assert_eq!(
            table["d3dmetal"]["overlay_dir"].as_str(),
            Some("wine/lib/external")
        );
        let probes = table["probes"].as_table().expect("probes");
        for key in ["cpuid", "dx11_sample", "xinput_check"] {
            assert!(probes.contains_key(key), "probes.{key}");
        }
        assert!(table["registry"]["hidraw_key"].is_str());
        assert_eq!(BundleSpec::parse(&text).expect("round trip"), spec());
    }

    #[test]
    fn optional_fields_round_trip() {
        let mut full = spec();
        full.wine.preloader = Some("wine/bin/wine-preloader".into());
        full.components.kosmickrisp = Some(component("25.3", "MIT"));
        full.overrides.d3dmetal.x86_64 = vec!["wine/lib/external/d3d11.dll".into()];
        full.entitlements.insert(
            "wine/bin/wine".into(),
            vec!["com.apple.security.cs.allow-jit".into()],
        );
        full.notarization_submission_id = Some("abc".into());
        let text = full.to_toml().expect("serializes");
        assert_eq!(BundleSpec::parse(&text).expect("round trip"), full);
    }

    #[test]
    fn a_bundle_without_d3dmetal_renders_no_table() {
        let mut none = spec();
        none.d3dmetal = None;
        let text = none.to_toml().expect("serializes");
        assert!(!text.contains("[d3dmetal]"), "{text}");
        assert_eq!(BundleSpec::parse(&text).expect("round trip"), none);
        let mut bad = none;
        bad.overrides.d3dmetal.x86_64 = vec!["wine/lib/external/d3d11.dll".into()];
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("[d3dmetal]"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_sync_mode_and_a_missing_loader() {
        let mut bad = spec();
        bad.sync_modes = vec!["fsync".into()];
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("fsync"), "{error}");
        let mut bad = spec();
        bad.sync_modes = vec!["esync".into(), "esync".into()];
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("more than once"), "{error}");
        let mut bad = spec();
        bad.wine.loader = String::new();
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("wine.loader"), "{error}");
    }

    #[test]
    fn rejects_what_the_reader_rejects() {
        let mut bad = spec();
        bad.id = "wine".into();
        assert!(bad.to_toml().unwrap_err().to_string().contains("`id`"));
        let mut bad = spec();
        bad.arch = "arm64".into();
        assert!(bad.to_toml().unwrap_err().to_string().contains("`arch`"));
        for version in ["11.0-1.partial", "V11", "11.", "11.0 beta", "latest"] {
            let mut bad = spec();
            bad.version = version.into();
            let error = bad.to_toml().unwrap_err();
            assert!(
                error.to_string().contains("`version`"),
                "{version}: {error}"
            );
        }
        let mut bad = spec();
        bad.overrides
            .dxmt
            .x86_64
            .push("dxmt/x86_64-windows/d3d11.so".into());
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains(".dll"), "{error}");
        let mut bad = spec();
        bad.overrides
            .dxmt
            .x86_64
            .push("dxmt/other/D3D11.DLL".into());
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("repeats"), "{error}");
        let mut bad = spec();
        bad.overrides.dxmt.x86_64.push("dxmt/../d3d9.dll".into());
        let error = bad.to_toml().unwrap_err();
        assert!(
            error.to_string().contains("overrides.dxmt.x86_64"),
            "{error}"
        );
        let mut bad = spec();
        bad.wine.wow64 = Wow64::None;
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("i386"), "{error}");
        bad.overrides.dxmt.i386.clear();
        bad.overrides.vkd3d.i386.clear();
        bad.to_toml().expect("64-bit only is valid");
    }

    #[test]
    fn rejects_absolute_and_escaping_paths() {
        let mut bad = spec();
        bad.wine.loader = "/usr/local/bin/wine".into();
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("wine.loader"), "{error}");
        let mut bad = spec();
        bad.probes.cpuid = "../cpuid".into();
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("probes.cpuid"), "{error}");
        let mut bad = spec();
        bad.wine.preloader = Some("wine/bin/../../x".into());
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("wine.preloader"), "{error}");
        let mut bad = spec();
        bad.d3dmetal = Some(D3DMetal {
            overlay_dir: "wine//lib".into(),
        });
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("overlay_dir"), "{error}");
    }

    /// The engine crate parses the same fixture with its reader, so the
    /// committed spec, the committed fixture, and the writer must agree.
    #[test]
    fn the_writer_reproduces_the_committed_bundle_fixture() {
        let xtask = Path::new(env!("CARGO_MANIFEST_DIR"));
        let spec_path = xtask.join("../build/bundle-spec.toml");
        let spec_text = fs::read_to_string(&spec_path).expect("build/bundle-spec.toml");
        let spec = BundleSpec::parse(&spec_text).expect("the committed spec validates");
        let fixture = xtask.join("tests/fixtures/BUNDLE.toml");
        let expected = fs::read_to_string(&fixture).expect("tests/fixtures/BUNDLE.toml");
        assert_eq!(
            spec.to_toml().expect("renders"),
            expected,
            "regenerate with: cargo xtask engine-payload --write-bundle-toml build/bundle-spec.toml xtask/tests/fixtures/BUNDLE.toml"
        );
        assert_eq!(BundleSpec::parse(&expected).expect("fixture parses"), spec);
        let notices = xtask.join("tests/fixtures/THIRD_PARTY_NOTICES.md");
        let expected = fs::read_to_string(&notices).expect("tests/fixtures/THIRD_PARTY_NOTICES.md");
        assert_eq!(spec.to_notices().expect("renders"), expected);
    }
}
