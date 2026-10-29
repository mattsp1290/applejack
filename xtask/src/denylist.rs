//! `build/denylist.toml`: path patterns that must never appear in a payload
//! or in a fetched archive (`D3DMetal` and its companions above all).

use std::fmt;

use glob::{MatchOptions, Pattern};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    deny: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    pattern: String,
    reason: String,
}

/// A compiled denylist.
#[derive(Debug)]
pub struct Denylist {
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    pattern: Pattern,
    source: String,
    reason: String,
}

/// One path that matched a rule.
#[derive(Debug, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    pub pattern: String,
    pub reason: String,
}

impl fmt::Display for Hit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} matches `{}`: {}",
            self.path, self.pattern, self.reason
        )
    }
}

#[derive(Debug)]
pub enum Error {
    Toml(toml::de::Error),
    Pattern { pattern: String, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(error) => write!(f, "denylist.toml: {error}"),
            Self::Pattern { pattern, message } => write!(f, "pattern `{pattern}`: {message}"),
        }
    }
}

impl std::error::Error for Error {}

// `*` never crosses a `/`, so `D3DMetal*` names one component and
// `lib/external/**` needs the literal directory.
const OPTIONS: MatchOptions = MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

impl Denylist {
    /// Parses and compiles the file text.
    ///
    /// # Errors
    ///
    /// Returns the TOML error or the first pattern that does not compile.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let file: File = toml::from_str(text).map_err(Error::Toml)?;
        let rules = file
            .deny
            .into_iter()
            .map(|entry| {
                let pattern = Pattern::new(&entry.pattern).map_err(|error| Error::Pattern {
                    pattern: entry.pattern.clone(),
                    message: error.msg.to_string(),
                })?;
                Ok(Rule {
                    pattern,
                    source: entry.pattern,
                    reason: entry.reason,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Reports every path in `paths` that matches a rule.
    ///
    /// A pattern without `/` is tested against each path component, so
    /// `D3DMetal*` catches the framework directory and the binary inside it.
    /// A pattern with `/` is tested against the whole path and against every
    /// suffix that starts at a component boundary, so `lib/external/**` catches
    /// `wine/lib/external/x`.
    pub fn scan<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Vec<Hit> {
        let mut hits = Vec::new();
        for path in paths {
            let normalized = path.trim_start_matches("./").trim_end_matches('/');
            for rule in &self.rules {
                if rule.matches(normalized) {
                    hits.push(Hit {
                        path: path.to_owned(),
                        pattern: rule.source.clone(),
                        reason: rule.reason.clone(),
                    });
                }
            }
        }
        hits
    }
}

impl Rule {
    fn matches(&self, path: &str) -> bool {
        if self.source.contains('/') {
            // `lib/external/**` also names the directory entry itself.
            let as_dir = format!("{path}/");
            suffixes(path)
                .chain(suffixes(&as_dir))
                .any(|suffix| self.pattern.matches_with(suffix, OPTIONS))
        } else {
            path.split('/')
                .any(|component| self.pattern.matches_with(component, OPTIONS))
        }
    }
}

fn suffixes(path: &str) -> impl Iterator<Item = &str> {
    std::iter::once(path).chain(
        path.match_indices('/')
            .map(move |(index, separator)| &path[index + separator.len()..]),
    )
}

#[cfg(test)]
mod tests {
    use super::Denylist;

    const FILE: &str = r#"
[[deny]]
pattern = "D3DMetal*"
reason = "Apple D3DMetal"

[[deny]]
pattern = "libd3dshared*"
reason = "Apple D3DMetal companion"

[[deny]]
pattern = "libmetalirconverter*"
reason = "Apple Metal shader converter"

[[deny]]
pattern = "lib/external/**"
reason = "GPTK external directory"
"#;

    #[test]
    fn catches_the_fixture_tree() {
        let denylist = Denylist::parse(FILE).expect("valid");
        assert_eq!(denylist.len(), 4);
        let listing = [
            "wine/bin/wine64",
            "wine/lib/libfreetype.6.dylib",
            "wine/lib/external/D3DMetal.framework/Versions/A/D3DMetal",
            "wine/lib/libd3dshared.dylib",
            "wine/lib/wine/x86_64-unix/winemetal.so",
        ];
        let hits = denylist.scan(listing);
        let paths: Vec<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
        assert!(paths.contains(&"wine/lib/external/D3DMetal.framework/Versions/A/D3DMetal"));
        assert!(paths.contains(&"wine/lib/libd3dshared.dylib"));
        assert!(!paths.contains(&"wine/bin/wine64"));
        assert!(!paths.contains(&"wine/lib/libfreetype.6.dylib"));
        assert!(!paths.contains(&"wine/lib/wine/x86_64-unix/winemetal.so"));
        let external = hits
            .iter()
            .filter(|hit| hit.path.contains("D3DMetal"))
            .map(|hit| hit.pattern.as_str())
            .collect::<Vec<_>>();
        assert!(external.contains(&"D3DMetal*"), "{external:?}");
        assert!(external.contains(&"lib/external/**"), "{external:?}");
    }

    #[test]
    fn directory_pattern_needs_the_literal_directory() {
        let denylist = Denylist::parse(FILE).expect("valid");
        assert_eq!(denylist.scan(["wine/lib/externally/x"]), []);
        assert_eq!(
            denylist
                .scan(["lib/external/libmetalirconverter.dylib"])
                .len(),
            2
        );
        assert_eq!(denylist.scan(["./wine/lib/external/"]).len(), 1);
    }

    #[test]
    fn rejects_an_unknown_field() {
        let text = "[[deny]]\npattern = \"a\"\nreason = \"b\"\nseverity = 1\n";
        assert!(Denylist::parse(text).is_err());
    }

    #[test]
    fn rejects_a_bad_pattern() {
        let text = "[[deny]]\npattern = \"a***\"\nreason = \"b\"\n";
        let error = Denylist::parse(text).unwrap_err();
        assert!(error.to_string().contains("a***"), "{error}");
    }
}
