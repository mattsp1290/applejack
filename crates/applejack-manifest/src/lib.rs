//! Manifest types and parser for `engines/manifest.toml`: the `[[engine]]`,
//! `[[tool]]`, and `[[probe]]` tables that pin every launcher-supplied
//! download by URL and SHA-256. Shared by `applejack-engine`, which embeds the
//! manifest, and by `xtask manifest-verify`, which validates it.
