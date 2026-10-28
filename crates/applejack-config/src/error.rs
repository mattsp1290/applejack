/// Errors from opening or migrating the configuration database.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite reported a failure.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Creating the database's parent directory failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The stored `schema_version` is not a non-negative integer.
    #[error("invalid schema_version in meta: {0:?}")]
    InvalidSchemaVersion(String),
    /// The database was written by a newer build with more migrations.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version recorded in the database.
        found: u32,
        /// Highest version this build knows how to create.
        supported: u32,
    },
}
