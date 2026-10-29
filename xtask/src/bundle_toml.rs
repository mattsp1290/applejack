//! `BUNDLE.toml` writer. The field set follows the engine plan; the engine
//! crate's own parser validates the written file once both land.

use std::collections::BTreeMap;
use std::fmt;

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
    pub overrides: Overrides,
    pub d3dmetal: D3DMetal,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Components {
    pub dxmt: Component,
    pub vkd3d_proton: Component,
    pub moltenvk: Component,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kosmickrisp: Option<Component>,
    pub gstreamer: Component,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub version: String,
    pub license: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub dxmt: DllList,
    pub vkd3d: DllList,
    pub d3dmetal: DllList,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DllList {
    pub dlls: Vec<String>,
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
            ("d3dmetal.overlay_dir", &self.d3dmetal.overlay_dir),
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
        if self.sync_modes.is_empty() {
            return Err(Error::Invalid("`sync_modes` is empty".into()));
        }
        if let Some(mode) = self
            .sync_modes
            .iter()
            .find(|mode| !SYNC_MODES.contains(&mode.as_str()))
        {
            return Err(Error::Invalid(format!("unknown sync mode `{mode}`")));
        }
        if !self.wine.source_url.starts_with("https://") {
            return Err(Error::Invalid("`wine.source_url` must be https".into()));
        }
        let paths = [
            ("wine.loader", Some(&self.wine.loader)),
            ("wine.server", Some(&self.wine.server)),
            ("wine.preloader", self.wine.preloader.as_ref()),
            ("d3dmetal.overlay_dir", Some(&self.d3dmetal.overlay_dir)),
            ("probes.cpuid", Some(&self.probes.cpuid)),
            ("probes.dx11_sample", Some(&self.probes.dx11_sample)),
            ("probes.xinput_check", Some(&self.probes.xinput_check)),
        ];
        for (name, value) in paths {
            if let Some(value) = value {
                relative_path(name, value)?;
            }
        }
        Ok(())
    }
}

/// A bundle path must stay inside the payload (or the overlay root): relative,
/// with no empty or `..` component. The same rule `pins.rs` applies to `dest`.
fn relative_path(name: &str, value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.starts_with('/')
        || value.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(Error::Invalid(format!(
            "`{name}` must be a relative path without `..` (got `{value}`)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        BundleSpec, Component, Components, D3DMetal, DllList, Overrides, Probes, Registry, Wine,
        Wow64,
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
            },
            overrides: Overrides {
                dxmt: DllList {
                    dlls: vec!["d3d11".into(), "dxgi".into(), "d3d10core".into()],
                },
                vkd3d: DllList {
                    dlls: vec!["d3d12".into(), "d3d12core".into()],
                },
                d3dmetal: DllList { dlls: vec![] },
            },
            d3dmetal: D3DMetal {
                overlay_dir: "wine/lib/external".into(),
            },
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
        for key in ["dxmt", "vkd3d_proton", "moltenvk", "gstreamer"] {
            let component = components[key].as_table().expect("component table");
            assert!(component.contains_key("version") && component.contains_key("license"));
        }
        assert!(!components.contains_key("kosmickrisp"));
        let overrides = table["overrides"].as_table().expect("overrides");
        for key in ["dxmt", "vkd3d", "d3dmetal"] {
            assert!(overrides[key]["dlls"].is_array(), "overrides.{key}.dlls");
        }
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
        full.entitlements.insert(
            "wine/bin/wine".into(),
            vec!["com.apple.security.cs.allow-jit".into()],
        );
        full.notarization_submission_id = Some("abc".into());
        let text = full.to_toml().expect("serializes");
        assert_eq!(BundleSpec::parse(&text).expect("round trip"), full);
    }

    #[test]
    fn rejects_an_unknown_sync_mode_and_a_missing_loader() {
        let mut bad = spec();
        bad.sync_modes = vec!["fsync".into()];
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("fsync"), "{error}");
        let mut bad = spec();
        bad.wine.loader = String::new();
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("wine.loader"), "{error}");
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
        bad.d3dmetal.overlay_dir = "wine//lib".into();
        let error = bad.to_toml().unwrap_err();
        assert!(error.to_string().contains("overlay_dir"), "{error}");
    }
}
