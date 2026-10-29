//! Per-game configuration database: the SQLite schema, migrations, presets,
//! and the resolve step that turns stored rows into launch settings. This is
//! the only SQLite handle in the workspace and only `applejack-core` depends
//! on it.
