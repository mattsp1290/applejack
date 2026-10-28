//! Manifest types and parser for `engines/manifest.toml`: the `[[engine]]`,
//! `[[tool]]`, and `[[probe]]` tables that pin every launcher-supplied
//! download by URL and SHA-256. Shared by `applejack-engine`, which embeds the
//! manifest, and by `xtask manifest-verify`, which validates it.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use serde::Deserialize;

/// Errors from parsing or validating a manifest.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The text is not valid TOML or does not match the manifest schema.
    ///
    /// `path` locates the offending value as `<table>[<index>].<field>`; it is
    /// `None` when the document failed to tokenize before any table was read.
    Parse {
        path: Option<String>,
        inner: toml::de::Error,
    },
    /// Two entries of the same table share an `id`.
    DuplicateId {
        table: &'static str,
        id: String,
        first: usize,
        second: usize,
    },
    /// A `sha256` value is not exactly 64 lowercase hexadecimal characters.
    InvalidSha256 { reason: String },
    /// A `url` value does not start with `https://` followed by a host.
    InvalidUrl { reason: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { path, inner } => {
                if let Some(path) = path {
                    write!(f, "manifest field `{path}`: ")?;
                }
                write!(f, "{inner}")
            }
            Self::DuplicateId {
                table,
                id,
                first,
                second,
            } => write!(
                f,
                "manifest table `{table}`: duplicate id `{id}` at [{first}] and [{second}]"
            ),
            Self::InvalidSha256 { reason } => {
                write!(
                    f,
                    "sha256 must be exactly 64 lowercase hex characters: {reason}"
                )
            }
            Self::InvalidUrl { reason } => {
                write!(
                    f,
                    "url must start with `https://` followed by a host: {reason}"
                )
            }
        }
    }
}

/// CPU architecture of a pinned artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "arm64")]
    Arm64,
}

/// Where a tool executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunsOn {
    /// A native macOS process.
    Host,
    /// A Windows process inside a Wine prefix.
    Prefix,
}

/// Where a probe executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunsIn {
    /// An x86-64 macOS process under Rosetta.
    Rosetta,
    /// A Windows process inside a Wine prefix.
    Prefix,
}

/// A SHA-256 digest: exactly 64 lowercase hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Sha256(String);

impl Sha256 {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha256 {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        let length = value.chars().count();
        if length != 64 {
            return Err(Error::InvalidSha256 {
                reason: format!("found {length} characters"),
            });
        }
        if let Some(bad) = value.chars().find(|c| !matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(Error::InvalidSha256 {
                reason: format!("found {bad:?}"),
            });
        }
        Ok(Self(value))
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A download URL: `https://` followed by a non-empty host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct HttpsUrl(String);

impl HttpsUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HttpsUrl {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        match value.strip_prefix("https://") {
            Some(rest) if !rest.is_empty() && !rest.starts_with('/') => Ok(Self(value)),
            Some(_) => Err(Error::InvalidUrl {
                reason: format!("no host in {value:?}"),
            }),
            None => Err(Error::InvalidUrl {
                reason: format!("found {value:?}"),
            }),
        }
    }
}

impl fmt::Display for HttpsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `[[engine]]` entry: a signed engine bundle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub id: String,
    pub version: String,
    pub arch: Arch,
    pub min_macos: String,
    pub url: HttpsUrl,
    pub sha256: Sha256,
    /// Component name to version, for example `wine` or `dxmt`.
    pub components: BTreeMap<String, String>,
    /// Empty until the bundle has been submitted for notarization.
    pub notarization_submission_id: String,
}

/// One `[[tool]]` entry: a helper executable the launcher downloads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub id: String,
    pub version: String,
    pub url: HttpsUrl,
    pub sha256: Sha256,
    #[serde(default)]
    pub arch: Option<Arch>,
    pub runs_on: RunsOn,
}

/// One `[[probe]]` entry: a diagnostic executable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub id: String,
    pub version: String,
    pub url: HttpsUrl,
    pub sha256: Sha256,
    pub arch: Arch,
    pub runs_in: RunsIn,
    /// When true the engine bundle carries the probe under `probes/` and
    /// `url` is informational.
    pub ships_in_bundle: bool,
}

/// The parsed `engines/manifest.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub engine: Vec<Engine>,
    #[serde(default)]
    pub tool: Vec<Tool>,
    #[serde(default)]
    pub probe: Vec<Probe>,
}

impl Manifest {
    /// Parses manifest text. Field-level rules (digest shape, URL scheme,
    /// enum values, unknown or missing fields) are enforced here; call
    /// [`Manifest::validate`] for cross-entry rules.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Parse`] naming the table, index, and field when the
    /// text is not valid TOML or does not match the schema.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let deserializer =
            toml::Deserializer::parse(text).map_err(|inner| Error::Parse { path: None, inner })?;
        serde_path_to_error::deserialize(deserializer).map_err(|error| {
            let path = error.path().to_string();
            Error::Parse {
                path: (path != ".").then_some(path),
                inner: error.into_inner(),
            }
        })
    }

    /// Checks cross-entry rules: every `id` is unique within its table.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateId`] for the first repeated id.
    pub fn validate(&self) -> Result<(), Error> {
        unique_ids(
            "engine",
            self.engine.iter().map(|engine| engine.id.as_str()),
        )?;
        unique_ids("tool", self.tool.iter().map(|tool| tool.id.as_str()))?;
        unique_ids("probe", self.probe.iter().map(|probe| probe.id.as_str()))?;
        Ok(())
    }
}

fn unique_ids<'a>(table: &'static str, ids: impl Iterator<Item = &'a str>) -> Result<(), Error> {
    let mut seen = HashMap::new();
    for (index, id) in ids.enumerate() {
        if let Some(first) = seen.insert(id, index) {
            return Err(Error::DuplicateId {
                table,
                id: id.to_owned(),
                first,
                second: index,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_an_empty_manifest() {
        let manifest = Manifest::parse("").expect("empty manifest parses");
        assert_eq!(manifest, Manifest::default());
        manifest.validate().expect("empty manifest validates");
    }

    #[test]
    fn sha256_accepts_only_64_lowercase_hex() {
        assert!(Sha256::try_from("a".repeat(64)).is_ok());
        assert!(Sha256::try_from("a".repeat(63)).is_err());
        assert!(Sha256::try_from("a".repeat(65)).is_err());
        assert!(Sha256::try_from("A".repeat(64)).is_err());
        assert!(Sha256::try_from("g".repeat(64)).is_err());
    }

    #[test]
    fn url_requires_https_and_a_host() {
        assert!(HttpsUrl::try_from("https://example.invalid/x".to_owned()).is_ok());
        assert!(HttpsUrl::try_from("http://example.invalid/x".to_owned()).is_err());
        assert!(HttpsUrl::try_from("https://".to_owned()).is_err());
        assert!(HttpsUrl::try_from("https:///x".to_owned()).is_err());
    }

    #[test]
    fn unknown_table_is_an_error() {
        let error = Manifest::parse("[[engines]]\nid = \"x\"\n").unwrap_err();
        assert!(matches!(error, Error::Parse { .. }), "{error}");
    }

    #[test]
    fn syntax_error_has_no_path() {
        let error = Manifest::parse("[[engine]\n").unwrap_err();
        assert!(matches!(error, Error::Parse { path: None, .. }), "{error}");
    }
}
