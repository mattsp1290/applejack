//! `build/pins.toml`: every source tarball, release asset, and toolchain the
//! engine payload build fetches, pinned by HTTPS URL and SHA-256.

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;

/// The parsed and validated pin file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    pub pin: Vec<Pin>,
}

/// One fetched input of the payload build.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub id: String,
    pub kind: PinKind,
    pub url: String,
    pub sha256: String,
    pub version: String,
    pub license: String,
    /// Where `--fetch` stores the archive, relative to `build/work/`.
    pub dest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinKind {
    Source,
    ReleaseAsset,
    Toolchain,
}

impl PinKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::ReleaseAsset => "release-asset",
            Self::Toolchain => "toolchain",
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Toml(toml::de::Error),
    Invalid { id: String, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(error) => write!(f, "pins.toml: {error}"),
            Self::Invalid { id, message } => write!(f, "pin `{id}`: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl Pins {
    /// Parses the file text and rejects entries without a usable hash, with
    /// a non-HTTPS URL, with an unsafe destination, or with a duplicate id or
    /// destination.
    ///
    /// # Errors
    ///
    /// Returns the first TOML or validation error.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let pins: Self = toml::from_str(text).map_err(Error::Toml)?;
        let mut ids = BTreeSet::new();
        let mut dests = BTreeSet::new();
        for pin in &pins.pin {
            pin.validate()?;
            if !ids.insert(pin.id.as_str()) {
                return Err(Error::Invalid {
                    id: pin.id.clone(),
                    message: "duplicate id".into(),
                });
            }
            if !dests.insert(pin.dest.as_str()) {
                return Err(Error::Invalid {
                    id: pin.id.clone(),
                    message: format!("dest `{}` is already used by another pin", pin.dest),
                });
            }
        }
        Ok(pins)
    }

    pub fn get(&self, id: &str) -> Option<&Pin> {
        self.pin.iter().find(|pin| pin.id == id)
    }
}

impl Pin {
    fn validate(&self) -> Result<(), Error> {
        let invalid = |message: &str| Error::Invalid {
            id: self.id.clone(),
            message: message.into(),
        };
        if self.id.is_empty() {
            return Err(invalid("empty id"));
        }
        if !self.url.starts_with("https://") {
            return Err(invalid("url must start with https://"));
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid("sha256 must be 64 lowercase hex digits"));
        }
        if self.version.is_empty() {
            return Err(invalid("empty version"));
        }
        if self.dest.is_empty()
            || self.dest.starts_with('/')
            || self
                .dest
                .split('/')
                .any(|part| part.is_empty() || part == "..")
        {
            return Err(invalid(
                "dest must be a relative path under build/work/ without `..`",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, PinKind, Pins};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn entry(id: &str, url: &str, sha256: &str, dest: &str) -> String {
        format!(
            "[[pin]]\nid = \"{id}\"\nkind = \"source\"\nurl = \"{url}\"\nsha256 = \"{sha256}\"\n\
             version = \"1.0\"\nlicense = \"MIT\"\ndest = \"{dest}\"\n"
        )
    }

    #[test]
    fn parses_a_valid_pin() {
        let pins = Pins::parse(&entry(
            "wine",
            "https://x/wine.tar.xz",
            HASH,
            "wine/a.tar.xz",
        ))
        .expect("valid");
        let pin = pins.get("wine").expect("present");
        assert_eq!(pin.kind, PinKind::Source);
        assert_eq!(pin.dest, "wine/a.tar.xz");
    }

    #[test]
    fn rejects_a_missing_hash() {
        let text = "[[pin]]\nid = \"a\"\nkind = \"source\"\nurl = \"https://x/a\"\n\
                    version = \"1\"\nlicense = \"MIT\"\ndest = \"a\"\n";
        assert!(matches!(Pins::parse(text), Err(Error::Toml(_))));
    }

    #[test]
    fn rejects_a_malformed_hash() {
        let upper = HASH.to_uppercase();
        let error = Pins::parse(&entry("a", "https://x/a", &upper, "a")).unwrap_err();
        assert!(error.to_string().contains("sha256"), "{error}");
        let short = Pins::parse(&entry("a", "https://x/a", &HASH[..63], "a")).unwrap_err();
        assert!(short.to_string().contains("sha256"), "{short}");
    }

    #[test]
    fn rejects_a_non_https_url() {
        let error = Pins::parse(&entry("a", "http://x/a", HASH, "a")).unwrap_err();
        assert!(error.to_string().contains("https"), "{error}");
    }

    #[test]
    fn rejects_a_duplicate_id() {
        let text = entry("a", "https://x/a", HASH, "a") + &entry("a", "https://x/b", HASH, "b");
        let error = Pins::parse(&text).unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
    }

    #[test]
    fn rejects_a_duplicate_dest() {
        let text = entry("a", "https://x/a", HASH, "same.tar")
            + &entry("b", "https://x/b", HASH, "same.tar");
        let error = Pins::parse(&text).unwrap_err();
        assert!(error.to_string().contains("pin `b`"), "{error}");
        assert!(error.to_string().contains("same.tar"), "{error}");
    }

    #[test]
    fn rejects_an_escaping_dest() {
        let error = Pins::parse(&entry("a", "https://x/a", HASH, "../a")).unwrap_err();
        assert!(error.to_string().contains("dest"), "{error}");
        let error = Pins::parse(&entry("a", "https://x/a", HASH, "/tmp/a")).unwrap_err();
        assert!(error.to_string().contains("dest"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_field() {
        let text = entry("a", "https://x/a", HASH, "a") + "mirror = \"https://y\"\n";
        assert!(matches!(Pins::parse(text.as_str()), Err(Error::Toml(_))));
    }
}
