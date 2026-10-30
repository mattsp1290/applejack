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
/// by formula name, or one of the [`SHIPPED_ELSEWHERE`] libraries Wine
/// binds to inside another component's tree. serde rejects
/// `deny_unknown_fields` together with `flatten` on the same struct, so
/// [`Components::validate`] is the strictness instead: a key that spells a
/// named row (`kosmic_krisp`, `MoltenVK`) or is no Homebrew formula name
/// fails the parse.
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
             that of the formula's command-line tools. A row whose location is a file inside\n\
             `wine/lib/GStreamer.framework` is a library the framework ships that Wine or a\n\
             copied library binds to directly: dyld keeps one image per install name per\n\
             process, so it is not copied again, and its license is its own, as listed, not\n\
             GStreamer's."
        );
        for elsewhere in SHIPPED_ELSEWHERE {
            if let Some(credit) = elsewhere.credit
                && self.components.libraries.contains_key(elsewhere.key)
            {
                let _ = writeln!(out);
                let _ = writeln!(out, "{credit}");
            }
        }
        let choices = self.components.choices();
        if !choices.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "Where a license above is an `OR` expression ({}), this bundle exercises\n\
                 the {EXERCISED_OPTION} option of that choice; any `AND` terms apply as written.",
                choices.join(", ")
            );
        }
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
            let location = match shipped_elsewhere(formula) {
                Some(elsewhere) => format!("`{}` ({})", elsewhere.file, elsewhere.why),
                None => format!(
                    "`wine/lib/` (x86-64 Homebrew formula `{formula}`, copied by the dylib closure)"
                ),
            };
            (formula.clone(), component, location)
        }));
        rows
    }

    /// The rows whose license is an `OR` expression, in notices order.
    fn choices(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .filter(|(_, component, _)| {
                Spdx::parse(&component.license).is_ok_and(|expression| expression.has_choice())
            })
            .map(|(name, _, _)| name)
            .collect()
    }

    /// Library rows are keyed by Homebrew formula name; a key that spells a
    /// named row is a typo of a pinned component. Every license parses as an
    /// SPDX expression, and every `OR` in it must list [`EXERCISED_OPTION`],
    /// the one the notices say the bundle takes, as one of its operands.
    fn validate(&self) -> Result<(), Error> {
        for (formula, component) in &self.libraries {
            if let Some(row) = named_row(formula) {
                return Err(Error::Invalid(format!(
                    "`components.{formula}` spells the pinned component row `{row}`; use that row"
                )));
            }
            if !is_formula_name(formula) {
                return Err(Error::Invalid(format!(
                    "`components.{formula}` is not a Homebrew formula name (`[A-Za-z0-9_+.-]+`, optionally `@<version>`)"
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
        for (name, component, _) in self.rows() {
            let expression = Spdx::parse(&component.license).map_err(|reason| {
                Error::Invalid(format!(
                    "the `{name}` license `{}` is not an SPDX expression: {reason}",
                    component.license
                ))
            })?;
            if !expression.every_choice_offers(EXERCISED_OPTION) {
                return Err(Error::Invalid(format!(
                    "the `{name}` license `{}` offers a choice without {EXERCISED_OPTION}, the option the notices state",
                    component.license
                )));
            }
        }
        Ok(())
    }
}

/// The named rows of [`Components`]; a library key that spells one of them
/// in any case, with or without `-` and `_`, is a typo, not a Homebrew
/// formula.
const NAMED_ROWS: [&str; 6] = [
    "dxmt",
    "vkd3d_proton",
    "moltenvk",
    "kosmickrisp",
    "gstreamer",
    "osslsigncode",
];

fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The named row `formula` spells, if any: such a formula (`molten-vk`) can
/// never have a library row.
pub(crate) fn named_row(formula: &str) -> Option<&'static str> {
    NAMED_ROWS
        .iter()
        .copied()
        .find(|row| fold(row) == fold(formula))
}

/// A library Wine, or a library the closure copied, binds to inside another
/// component's tree rather than under `wine/lib/`: it has a `[components]`
/// row like a copied library (the engine reader accepts nothing richer), and
/// the notices render its real location, provenance, and any credit its
/// license requires. The closure's `SONAME_POLICY` names the same files.
pub(crate) struct Elsewhere {
    /// The `[components]` key.
    pub(crate) key: &'static str,
    /// The payload-relative file.
    pub(crate) file: &'static str,
    /// What the file is and who binds to it.
    pub(crate) why: &'static str,
    /// Text the license requires in the distribution's documentation.
    pub(crate) credit: Option<&'static str>,
}

pub(crate) const SHIPPED_ELSEWHERE: &[Elsewhere] = &[
    Elsewhere {
        key: "freetype",
        file: "wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib",
        why: "FreeType as the GStreamer framework ships it (`LC_ID_DYLIB` current version 26.2.20); Wine's font engine, which `win32u` and `dwrite` load by name through the link `wine/lib/libfreetype.6.dylib`",
        // The FTL's redistribution clause asks for a statement that the
        // software is based in part on the FreeType Team's work, and its
        // suggested wording names the year of the release used (2.13.3 is
        // the 2024 release).
        credit: Some(
            "FreeType is used under the FreeType License (`FTL`); FreeType's GPL-2.0-or-later\n\
             alternative is not elected. This software is based in part on the work of the\n\
             FreeType Team. Portions of this software are copyright © 2024 The FreeType\n\
             Project (www.freetype.org). All rights reserved.",
        ),
    },
    Elsewhere {
        key: "proxy-libintl",
        file: "wine/lib/GStreamer.framework/Versions/1.0/lib/libintl.8.dylib",
        why: "proxy-libintl as the GStreamer framework ships it (`LC_ID_DYLIB` current version 10.5.0), a stub gettext runtime whose `dgettext` returns the message untranslated; the copied gnutls and libidn2 bind to it, so their messages are English",
        credit: None,
    },
];

/// The [`SHIPPED_ELSEWHERE`] entry for a `[components]` key.
pub(crate) fn shipped_elsewhere(key: &str) -> Option<&'static Elsewhere> {
    SHIPPED_ELSEWHERE
        .iter()
        .find(|elsewhere| elsewhere.key == key)
}

/// The option this bundle exercises where a component's license is an `OR`
/// expression: the LGPL beside Wine's own LGPL-2.1-or-later.
pub const EXERCISED_OPTION: &str = "LGPL-3.0-or-later";

/// An SPDX license expression, as far as the notices need it: `AND` binds
/// tighter than `OR`, parentheses group, and `WITH` qualifies one license.
#[derive(Debug, PartialEq, Eq)]
enum Spdx {
    License(String),
    And(Vec<Spdx>),
    Or(Vec<Spdx>),
}

impl Spdx {
    fn parse(text: &str) -> Result<Self, String> {
        let mut tokens: Vec<&str> = Vec::new();
        for word in text.split_whitespace() {
            let mut rest = word;
            while let Some(after) = rest.strip_prefix('(') {
                tokens.push("(");
                rest = after;
            }
            let mut closing = 0;
            while let Some(before) = rest.strip_suffix(')') {
                closing += 1;
                rest = before;
            }
            if !rest.is_empty() {
                tokens.push(rest);
            }
            tokens.extend(std::iter::repeat_n(")", closing));
        }
        let mut cursor = 0;
        let expression = Self::disjunction(&tokens, &mut cursor)?;
        match tokens.get(cursor) {
            None => Ok(expression),
            Some(token) => Err(format!("unexpected `{token}`")),
        }
    }

    fn disjunction(tokens: &[&str], cursor: &mut usize) -> Result<Self, String> {
        let mut operands = vec![Self::conjunction(tokens, cursor)?];
        while tokens.get(*cursor) == Some(&"OR") {
            *cursor += 1;
            operands.push(Self::conjunction(tokens, cursor)?);
        }
        Ok(Self::group(operands, Self::Or))
    }

    fn conjunction(tokens: &[&str], cursor: &mut usize) -> Result<Self, String> {
        let mut operands = vec![Self::primary(tokens, cursor)?];
        while tokens.get(*cursor) == Some(&"AND") {
            *cursor += 1;
            operands.push(Self::primary(tokens, cursor)?);
        }
        Ok(Self::group(operands, Self::And))
    }

    fn group(mut operands: Vec<Self>, make: fn(Vec<Self>) -> Self) -> Self {
        if operands.len() == 1 {
            operands.remove(0)
        } else {
            make(operands)
        }
    }

    fn primary(tokens: &[&str], cursor: &mut usize) -> Result<Self, String> {
        let is_word = |token: &str| !matches!(token, "(" | ")" | "AND" | "OR" | "WITH");
        match tokens.get(*cursor).copied() {
            None => Err("the expression ends where a license was expected".into()),
            Some("(") => {
                *cursor += 1;
                let inner = Self::disjunction(tokens, cursor)?;
                if tokens.get(*cursor) != Some(&")") {
                    return Err("a `(` without its `)`".into());
                }
                *cursor += 1;
                Ok(inner)
            }
            Some(token) if !is_word(token) => Err(format!("unexpected `{token}`")),
            Some(license) => {
                *cursor += 1;
                if tokens.get(*cursor) == Some(&"WITH") {
                    let exception = tokens
                        .get(*cursor + 1)
                        .copied()
                        .filter(|token| is_word(token))
                        .ok_or_else(|| "`WITH` without an exception".to_owned())?;
                    *cursor += 2;
                    return Ok(Self::License(format!("{license} WITH {exception}")));
                }
                Ok(Self::License(license.to_owned()))
            }
        }
    }

    fn has_choice(&self) -> bool {
        match self {
            Self::License(_) => false,
            Self::And(operands) => operands.iter().any(Self::has_choice),
            Self::Or(_) => true,
        }
    }

    /// Every `OR` in the expression lists `option` as one of its own
    /// operands, so electing it is always possible.
    fn every_choice_offers(&self, option: &str) -> bool {
        match self {
            Self::License(_) => true,
            Self::And(operands) => operands
                .iter()
                .all(|operand| operand.every_choice_offers(option)),
            Self::Or(operands) => {
                operands
                    .iter()
                    .any(|operand| *operand == Self::License(option.to_owned()))
                    && operands
                        .iter()
                        .all(|operand| operand.every_choice_offers(option))
            }
        }
    }
}

/// Homebrew's formula-name grammar, `[A-Za-z0-9_+.-]+` with an optional
/// `@[A-Za-z0-9.]+` version suffix (`openssl@3`, `icu4c@77`). TOML quotes a
/// key with `@`, `+`, or `.` by itself.
pub(crate) fn is_formula_name(name: &str) -> bool {
    let (base, version) = name
        .split_once('@')
        .map_or((name, None), |(base, version)| (base, Some(version)));
    !base.is_empty()
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-'))
        && version.is_none_or(|version| {
            !version.is_empty()
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.')
        })
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
    fn a_versioned_formula_row_round_trips_as_a_quoted_key() {
        let mut spec = spec();
        spec.components
            .libraries
            .insert("openssl@3".into(), component("3.5.1", "Apache-2.0"));
        spec.components
            .libraries
            .insert("c++.utils".into(), component("1", "MIT"));
        let text = spec.to_toml().expect("renders");
        assert!(text.contains("\"openssl@3\""), "{text}");
        assert!(text.contains("\"c++.utils\""), "{text}");
        assert_eq!(BundleSpec::parse(&text).expect("parses"), spec);
        let notices = spec.to_notices().expect("renders");
        assert!(
            notices.contains("| openssl@3 | 3.5.1 | Apache-2.0 |"),
            "{notices}"
        );
        for name in ["openssl@", "@3", "a b", "", "x@1@2", "gnu/tls"] {
            let mut bad = spec.clone();
            bad.components
                .libraries
                .insert(name.into(), component("1", "MIT"));
            let error = bad.to_toml().unwrap_err();
            assert!(
                error.to_string().contains("not a Homebrew formula name"),
                "{name:?}: {error}"
            );
        }
    }

    #[test]
    fn a_typod_named_row_is_refused_rather_than_rendered_as_a_library() {
        for typo in ["kosmic_krisp", "KosmicKrisp", "vkd3d-proton", "MoltenVK"] {
            let text = format!(
                "{}\n[components.\"{typo}\"]\nversion = \"9.9\"\nlicense = \"MIT\"\n",
                spec().to_toml().expect("renders")
            );
            let error = BundleSpec::parse(&text).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("spells the pinned component row"),
                "{typo}: {error}"
            );
        }
        let mut bad = spec();
        bad.components
            .libraries
            .insert("dxmt2".into(), component("1", "MIT"));
        assert!(bad.to_toml().is_ok(), "a different name is a formula");
    }

    #[test]
    fn an_or_license_names_the_exercised_option_in_the_notices() {
        assert!(
            !spec()
                .to_notices()
                .expect("renders")
                .contains("OR` expression")
        );
        let mut dual = spec();
        dual.components.libraries.insert(
            "gmp".into(),
            component("6.3.0", "LGPL-3.0-or-later OR GPL-2.0-or-later"),
        );
        dual.components.libraries.insert(
            "libidn2".into(),
            component(
                "2.3.8",
                "(GPL-2.0-or-later OR LGPL-3.0-or-later) AND Unicode-DFS-2016",
            ),
        );
        let notices = dual.to_notices().expect("renders");
        assert!(
            notices.contains(
                "Where a license above is an `OR` expression (gmp, libidn2), this bundle exercises\nthe LGPL-3.0-or-later option of that choice; any `AND` terms apply as written."
            ),
            "{notices}"
        );
        let mut bad = dual;
        bad.components
            .libraries
            .insert("x".into(), component("1", "MIT OR GPL-2.0-only"));
        let error = bad.to_notices().unwrap_err();
        assert!(error.to_string().contains("offers a choice"), "{error}");
    }

    /// The option must be an operand of every `OR` in the parsed expression,
    /// not a substring of it; a malformed expression is refused too.
    #[test]
    fn the_exercised_option_must_be_an_operand_of_every_or() {
        let with = |license: &str| {
            let mut spec = spec();
            spec.components
                .libraries
                .insert("x".into(), component("1", license));
            spec.to_notices()
        };
        for real in [
            "LGPL-3.0-or-later OR GPL-2.0-or-later",
            "GPL-2.0-or-later OR LGPL-3.0-or-later",
            "(GPL-2.0-or-later OR LGPL-3.0-or-later) AND Unicode-DFS-2016",
            "LGPL-2.1-or-later",
            "FTL",
            "GPL-2.0-or-later WITH Classpath-exception-2.0 OR LGPL-3.0-or-later",
        ] {
            with(real).unwrap_or_else(|error| panic!("{real}: {error}"));
        }
        for bad in [
            "LGPL-3.0-or-later AND (MIT OR Apache-2.0)",
            "GPL-2.0-only OR (MIT AND LGPL-3.0-or-later)",
            "MIT OR GPL-2.0-only",
            "LGPL-3.0-or-later-ish OR MIT",
        ] {
            let error = with(bad).unwrap_err();
            assert!(
                error.to_string().contains("offers a choice without"),
                "{bad}: {error}"
            );
        }
        for malformed in [
            "(MIT",
            "MIT OR",
            "MIT AND OR GPL-2.0-only",
            "MIT WITH",
            "MIT) OR LGPL-3.0-or-later",
        ] {
            let error = with(malformed).unwrap_err();
            assert!(
                error.to_string().contains("not an SPDX expression"),
                "{malformed}: {error}"
            );
        }
        let mut nested = spec();
        nested.components.libraries.insert(
            "a".into(),
            component("1", "(GPL-2.0-or-later OR LGPL-3.0-or-later) AND MIT"),
        );
        nested
            .components
            .libraries
            .insert("b".into(), component("1", "MIT"));
        assert!(
            nested
                .to_notices()
                .expect("renders")
                .contains("`OR` expression (a), this bundle"),
        );
    }

    /// A library Wine binds to inside another component's tree renders its
    /// real location and, where its license asks for one, the credit.
    #[test]
    fn a_library_shipped_elsewhere_renders_its_location_and_credit() {
        let mut elsewhere = spec();
        elsewhere
            .components
            .libraries
            .insert("freetype".into(), component("2.13.3", "FTL"));
        elsewhere
            .components
            .libraries
            .insert("proxy-libintl".into(), component("1", "LGPL-2.0-or-later"));
        let notices = elsewhere.to_notices().expect("renders");
        assert!(
            notices.contains(
                "| freetype | 2.13.3 | FTL | `wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib` (FreeType as the GStreamer framework ships it"
            ),
            "{notices}"
        );
        assert!(
            notices.contains(
                "| proxy-libintl | 1 | LGPL-2.0-or-later | `wine/lib/GStreamer.framework/Versions/1.0/lib/libintl.8.dylib` (proxy-libintl as the GStreamer framework ships it"
            ),
            "{notices}"
        );
        assert!(
            notices.contains("Portions of this software are copyright © 2024 The FreeType"),
            "{notices}"
        );
        assert!(!notices.contains("copied by the dylib closure)\n| freetype"));
        assert!(!spec().to_notices().expect("renders").contains("FreeType"));
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
        for elsewhere in super::SHIPPED_ELSEWHERE {
            assert!(
                spec.components.libraries.contains_key(elsewhere.key),
                "build/bundle-spec.toml has no [components.{}] row for {}",
                elsewhere.key,
                elsewhere.file
            );
        }
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
