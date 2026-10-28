//! Engine abstraction: the `Engine` trait, the on-disk engine bundle layout
//! and `BUNDLE.toml`, engine install and rollback, prefix creation and
//! snapshots, and launch. Accepts and returns plain structs and never depends
//! on `applejack-config`.
