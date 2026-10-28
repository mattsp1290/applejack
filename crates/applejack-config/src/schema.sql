-- Schema version 1. Column lists come from the per-game configuration design;
-- later changes are new migrations, never edits to this file.

CREATE TABLE apps (
    appid INTEGER PRIMARY KEY,
    name TEXT,
    installdir TEXT,
    drm TEXT CHECK (drm IN ('unknown', 'none', 'steam')),
    library_dir TEXT,
    last_installed_buildid INTEGER,
    last_manifest_ids TEXT,
    fetched_at TEXT
);

CREATE TABLE game_config (
    appid INTEGER PRIMARY KEY,
    engine_id TEXT,
    engine_version TEXT,
    graphics_backend TEXT,
    sync_mode TEXT,
    hidraw_enabled INTEGER,
    steam_input INTEGER,
    steam_overlay INTEGER,
    rosetta_advertise_avx INTEGER,
    extra_env TEXT,
    launch_mode TEXT CHECK (launch_mode IN ('client', 'direct')),
    launch_exe_override TEXT,
    launch_args_override TEXT,
    direct_verified_at TEXT,
    isolated_prefix INTEGER,
    dx12_expectation TEXT,
    notes TEXT
);

CREATE TABLE presets (
    id INTEGER PRIMARY KEY,
    appid INTEGER,
    source TEXT,
    author TEXT,
    engine_id TEXT,
    engine_version TEXT,
    host_chip TEXT,
    macos_version TEXT,
    rating INTEGER,
    payload TEXT,
    imported_at TEXT
);

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    appid INTEGER,
    started_at TEXT,
    ended_at TEXT,
    engine_id TEXT,
    engine_version TEXT,
    exit_code INTEGER,
    avg_fps REAL,
    log_path TEXT,
    frame_capture_path TEXT,
    frame_owner_exe TEXT
);

CREATE TABLE engine_versions (
    engine_id TEXT NOT NULL,
    version TEXT NOT NULL,
    installed_at TEXT,
    active INTEGER,
    PRIMARY KEY (engine_id, version)
);

CREATE TABLE d3dmetal (
    engine_version TEXT NOT NULL PRIMARY KEY,
    imported_from TEXT,
    gptk_version TEXT,
    accepted_at TEXT
);

CREATE TABLE meta (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT
);
