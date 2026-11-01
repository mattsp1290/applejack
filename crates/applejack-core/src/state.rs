//! [`Core`]: the state the CLI and the RPC handler share. Both call the same
//! methods (`engines`, `prefixes`, `game_config`), so they cannot drift.
//! Nothing on disk is touched until a method needs it: `hello` and
//! `host.doctor` work with an unusable data directory, and only a write
//! creates `db.sqlite`.

use std::path::{Path, PathBuf};

use applejack_engine::{EngineLayout, RosettaWine};

use crate::config::{self, ConfigDb, DataDirError};
use crate::error::CoreError;

/// Resolved directories, the engine, and the database handle.
#[derive(Debug)]
pub struct Core {
    data_dir: PathBuf,
    log_dir: PathBuf,
    home: Option<PathBuf>,
    layout: EngineLayout,
    pub(crate) engine: RosettaWine,
    allow_unsigned: bool,
    db: Option<ConfigDb>,
}

impl Core {
    /// A core over `data_dir` with logs under it and unsigned installs off.
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            log_dir: data_dir.join("logs"),
            home: None,
            layout: EngineLayout::new(data_dir.clone()),
            data_dir,
            engine: RosettaWine,
            allow_unsigned: false,
            db: None,
        }
    }

    /// The core this process's environment describes.
    ///
    /// # Errors
    ///
    /// Returns [`DataDirError`] when no absolute data directory is set.
    pub fn from_env() -> Result<Self, DataDirError> {
        let env = |name: &str| std::env::var_os(name);
        let mut core = Self::new(config::data_dir_from(env)?);
        core.log_dir = config::log_dir_from(&core.data_dir, env);
        core.home = config::home_dir_from(env);
        core.allow_unsigned = config::allow_unsigned_from(env);
        Ok(core)
    }

    /// Sets whether `engine install --from` is allowed.
    #[must_use]
    pub fn with_unsigned_installs(mut self, allow: bool) -> Self {
        self.allow_unsigned = allow;
        self
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Log root; sessions write under `<log_dir>/<session-id>/`.
    #[must_use]
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    #[must_use]
    pub fn layout(&self) -> &EngineLayout {
        &self.layout
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub(crate) const fn allows_unsigned(&self) -> bool {
        self.allow_unsigned
    }

    /// A copy with its own database handle, for a job thread: a connection
    /// stays on the thread that opened it.
    #[must_use]
    pub fn detached(&self) -> Self {
        Self {
            data_dir: self.data_dir.clone(),
            log_dir: self.log_dir.clone(),
            home: self.home.clone(),
            layout: self.layout.clone(),
            engine: self.engine,
            allow_unsigned: self.allow_unsigned,
            db: None,
        }
    }

    /// The database, created on first use. Only a write goes through here.
    pub(crate) fn db(&mut self) -> Result<&ConfigDb, CoreError> {
        if self.db.is_none() {
            self.db = Some(config::open_db(&self.data_dir)?);
        }
        self.db
            .as_ref()
            .ok_or_else(|| CoreError::Message("the database did not open".to_owned()))
    }

    /// Whether `db.sqlite` exists, so a write plan can tell a real database
    /// from the empty stand-in [`Core::reading`] offers.
    fn db_exists(&self) -> bool {
        self.db.is_some() || self.data_dir.join(config::DB_FILE).exists()
    }

    /// Runs `read` over the database. A missing one holds no rows, so the
    /// reader gets the same answer from an empty in-memory one and
    /// `db.sqlite` is not created.
    pub(crate) fn reading<T>(
        &mut self,
        read: impl FnOnce(&ConfigDb) -> Result<T, CoreError>,
    ) -> Result<T, CoreError> {
        if self.db_exists() {
            read(self.db()?)
        } else {
            let scratch = ConfigDb::open_in_memory()?;
            read(&scratch)
        }
    }
}
