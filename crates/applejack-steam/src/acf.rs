//! `appmanifest_<appid>.acf`: the file the Windows Steam client reads to adopt
//! an installed title.
//!
//! Field names follow the public format as documented by `SteamKit` and `SteamDB`.
//! The committed fixture is hand-written and redacted; work package 5 replaces
//! it with a captured client file, and the writer's key set and order are
//! revisited then.

use crate::Error;
use crate::vdf::{Document, Node, Value};

/// `StateFlags` value the Steam client writes for a fully installed title.
pub const STATE_FLAGS_FULLY_INSTALLED: u32 = 4;

const ROOT_KEY: &str = "AppState";

/// One entry of `InstalledDepots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub size: u64,
}

/// The fields applejack reads and writes in an `appmanifest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    pub appid: u32,
    pub name: String,
    pub installdir: String,
    pub state_flags: u32,
    pub buildid: u64,
    pub last_updated: u64,
    pub size_on_disk: u64,
    pub installed_depots: Vec<InstalledDepot>,
}

impl AppManifest {
    /// Reads one `appmanifest` file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VdfParse`] for malformed text and [`Error::AcfField`]
    /// when a required field is missing or not a number.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let document = Document::parse(text)?;
        let root = document.get(ROOT_KEY).ok_or_else(|| missing(ROOT_KEY))?;
        let installed_depots = match root.get("InstalledDepots") {
            Some(Value::Map(children)) => children
                .iter()
                .map(|node| {
                    let path = format!("InstalledDepots/{}", node.key);
                    Ok(InstalledDepot {
                        depot_id: node.key.parse().map_err(|_| not_a_number(&path))?,
                        manifest_id: number(&node.value, &path, "manifest")?,
                        size: number(&node.value, &path, "size")?,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?,
            Some(Value::Str(_)) => {
                return Err(Error::AcfField {
                    path: "InstalledDepots".to_owned(),
                    reason: "expected a map".to_owned(),
                });
            }
            None => Vec::new(),
        };
        Ok(Self {
            appid: number(root, "", "appid")?
                .try_into()
                .map_err(|_| not_a_number("appid"))?,
            name: text_field(root, "name")?,
            installdir: text_field(root, "installdir")?,
            state_flags: number(root, "", "StateFlags")?
                .try_into()
                .map_err(|_| not_a_number("StateFlags"))?,
            buildid: number(root, "", "buildid")?,
            last_updated: number(root, "", "LastUpdated")?,
            size_on_disk: number(root, "", "SizeOnDisk")?,
            installed_depots,
        })
    }

    /// True when the Steam client would consider the title fully installed;
    /// any other `StateFlags` means the client owns the title's update.
    #[must_use]
    pub fn is_fully_installed(&self) -> bool {
        self.state_flags == STATE_FLAGS_FULLY_INSTALLED
    }

    /// `appmanifest_<appid>.acf`
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("appmanifest_{}.acf", self.appid)
    }

    /// The `KeyValues` tree the writer renders.
    #[must_use]
    pub fn to_document(&self) -> Document {
        let depots = self
            .installed_depots
            .iter()
            .map(|depot| {
                Node::map(
                    depot.depot_id.to_string(),
                    vec![
                        Node::str("manifest", depot.manifest_id.to_string()),
                        Node::str("size", depot.size.to_string()),
                    ],
                )
            })
            .collect();
        Document {
            nodes: vec![Node::map(
                ROOT_KEY,
                vec![
                    Node::str("appid", self.appid.to_string()),
                    Node::str("name", self.name.clone()),
                    Node::str("StateFlags", self.state_flags.to_string()),
                    Node::str("installdir", self.installdir.clone()),
                    Node::str("LastUpdated", self.last_updated.to_string()),
                    Node::str("SizeOnDisk", self.size_on_disk.to_string()),
                    Node::str("buildid", self.buildid.to_string()),
                    Node::map("InstalledDepots", depots),
                ],
            )],
        }
    }
}

/// Renders an [`AppManifest`] as `appmanifest_<appid>.acf` text.
#[derive(Debug, Clone, Copy)]
pub struct AcfWriter<'a> {
    manifest: &'a AppManifest,
}

impl<'a> AcfWriter<'a> {
    #[must_use]
    pub fn new(manifest: &'a AppManifest) -> Self {
        Self { manifest }
    }

    #[must_use]
    pub fn write(&self) -> String {
        self.manifest.to_document().to_text()
    }
}

fn text_field(root: &Value, key: &str) -> Result<String, Error> {
    root.get_str(key)
        .map(str::to_owned)
        .ok_or_else(|| missing(key))
}

fn number(value: &Value, prefix: &str, key: &str) -> Result<u64, Error> {
    let path = if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}/{key}")
    };
    let text = value.get_str(key).ok_or_else(|| missing(&path))?;
    text.parse().map_err(|_| not_a_number(&path))
}

fn missing(path: &str) -> Error {
    Error::AcfField {
        path: path.to_owned(),
        reason: "missing".to_owned(),
    }
}

fn not_a_number(path: &str) -> Error {
    Error::AcfField {
        path: path.to_owned(),
        reason: "not an unsigned integer".to_owned(),
    }
}
