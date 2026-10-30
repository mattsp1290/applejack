use crate::presets::Confirmation;

/// Errors from the configuration database, presets, and field parsing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite reported a failure.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Creating the database's parent directory failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON column or preset payload could not be encoded or decoded.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The stored `schema_version` is not a non-negative integer.
    #[error("invalid schema_version in meta: {0:?}")]
    InvalidSchemaVersion(String),
    /// SQLite kept a journal mode other than WAL for a file database.
    #[error("expected journal_mode wal, sqlite reports {found:?}")]
    JournalMode {
        /// Mode SQLite reported after the switch.
        found: String,
    },
    /// The database was written by a newer build with more migrations.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version recorded in the database.
        found: u32,
        /// Highest version this build knows how to create.
        supported: u32,
    },
    /// `config set` named a key that is not a `game_config` column.
    #[error("unknown config field {0:?}; known fields: {known}", known = crate::Field::names().join(", "))]
    UnknownField(String),
    /// A value does not parse for its typed field, in the CLI or in a stored row.
    #[error("invalid value {value:?} for {field}: {reason}")]
    InvalidValue {
        /// Column the value was meant for.
        field: &'static str,
        /// The offending text.
        value: String,
        /// Why it was refused.
        reason: String,
    },
    /// The preset file declares a schema version this build does not read.
    #[error("unsupported preset schema version {0}; this build reads version {supported}", supported = crate::presets::SCHEMA_VERSION)]
    UnsupportedPresetSchema(u64),
    /// An exported text field still names the account after host paths were
    /// rewritten to placeholders.
    #[error("{field} names the account in a form export cannot rewrite; clear or edit it first")]
    AccountNameInExport {
        /// The field, as `show` and the import report name it.
        field: String,
    },
    /// `ImportPlan::apply` ran without the confirmation its items require.
    #[error("import needs confirmation for: {items}", items = render_confirmations(.0))]
    ConfirmationRequired(Vec<Confirmation>),
}

fn render_confirmations(items: &[Confirmation]) -> String {
    items
        .iter()
        // Preset values are untrusted; Debug formatting escapes control
        // characters before the text reaches a log or the RPC layer.
        .map(|item| format!("{}={:?}", item.field, item.value))
        .collect::<Vec<_>>()
        .join(", ")
}
