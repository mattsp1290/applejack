//! JSON-RPC 2.0 envelope types shared by the serve loop, the goldens, and the
//! Swift mirrors.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;

/// The `jsonrpc` member. Only `"2.0"` is accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Version;

impl Version {
    pub const TEXT: &'static str = "2.0";
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Self::TEXT)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = Cow::<str>::deserialize(deserializer)?;
        if text == Self::TEXT {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "jsonrpc must be \"{}\", got \"{text}\"",
                Self::TEXT
            )))
        }
    }
}

/// A request id: JSON-RPC allows a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::String(text) => write!(f, "{text:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: Id,
    pub method: String,
    /// Missing `params` reads as `{}` so methods without parameters need none.
    #[serde(default = "empty_params")]
    pub params: Value,
}

impl Request {
    #[must_use]
    pub fn new(id: Id, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            method: method.into(),
            params,
        }
    }
}

fn empty_params() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: Version,
    /// `None` (serialized as `null`) when the request id could not be read.
    pub id: Option<Id>,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    Result(Value),
    Error(RpcError),
}

impl Response {
    #[must_use]
    pub fn result(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id: Some(id),
            body: Body::Result(result),
        }
    }

    #[must_use]
    pub fn error(id: Option<Id>, error: RpcError) -> Self {
        Self {
            jsonrpc: Version,
            id,
            body: Body::Error(error),
        }
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.body, Body::Result(_))
    }
}

/// A server-to-shell message with no id and no reply (`job.*`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    pub params: Value,
}

impl Notification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: Version,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.code.name(),
            self.code.code(),
            self.message
        )
    }
}

impl std::error::Error for RpcError {}

/// Error codes on the wire. The five JSON-RPC codes keep their reserved
/// values; application codes start at 1000. The table in
/// `docs/rpc-protocol.md` and `tests/rpc_golden/error_codes.json` are
/// rendered from [`ErrorCode::ALL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    /// The serve loop itself failed, for example a result could not be encoded.
    InternalError,
    /// `hello` carried a `protocol_version` the core does not speak.
    ProtocolMismatch,
    /// A request arrived before a successful `hello`.
    HelloRequired,
    /// The method is in the table but its work package has not landed.
    NotImplemented,
    /// The method's own work failed (I/O, database, child process).
    Internal,
    /// A `job.failed` whose failure has no reason code of its own.
    JobFailed,
    /// The method needs an engine and none is installed or active.
    EngineNotInstalled,
    /// An archive does not hash to the digest it must have.
    ChecksumMismatch,
    /// An archive or its `BUNDLE.toml` breaks a bundle rule.
    InvalidBundle,
    /// The disk filled up.
    DiskFull,
    /// No published version exists, or this build cannot fetch it.
    DownloadFailed,
    /// A local bundle was requested without `APPLEJACK_ALLOW_UNSIGNED_ENGINE=1`.
    UnsignedEngineRefused,
}

impl ErrorCode {
    pub const ALL: [Self; 16] = [
        Self::ParseError,
        Self::InvalidRequest,
        Self::MethodNotFound,
        Self::InvalidParams,
        Self::InternalError,
        Self::ProtocolMismatch,
        Self::HelloRequired,
        Self::NotImplemented,
        Self::Internal,
        Self::JobFailed,
        Self::EngineNotInstalled,
        Self::ChecksumMismatch,
        Self::InvalidBundle,
        Self::DiskFull,
        Self::DownloadFailed,
        Self::UnsignedEngineRefused,
    ];

    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::ProtocolMismatch => 1000,
            Self::HelloRequired => 1001,
            Self::NotImplemented => 1002,
            Self::Internal => 1003,
            Self::JobFailed => 1004,
            Self::EngineNotInstalled => 1005,
            Self::ChecksumMismatch => 1006,
            Self::InvalidBundle => 1007,
            Self::DiskFull => 1008,
            Self::DownloadFailed => 1009,
            Self::UnsignedEngineRefused => 1010,
        }
    }

    #[must_use]
    pub fn from_code(code: i64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.code() == code)
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ParseError => "ParseError",
            Self::InvalidRequest => "InvalidRequest",
            Self::MethodNotFound => "MethodNotFound",
            Self::InvalidParams => "InvalidParams",
            Self::InternalError => "InternalError",
            Self::ProtocolMismatch => "ProtocolMismatch",
            Self::HelloRequired => "HelloRequired",
            Self::NotImplemented => "NotImplemented",
            Self::Internal => "Internal",
            Self::JobFailed => "JobFailed",
            Self::EngineNotInstalled => "EngineNotInstalled",
            Self::ChecksumMismatch => "ChecksumMismatch",
            Self::InvalidBundle => "InvalidBundle",
            Self::DiskFull => "DiskFull",
            Self::DownloadFailed => "DownloadFailed",
            Self::UnsignedEngineRefused => "UnsignedEngineRefused",
        }
    }

    /// The meaning column of the docs table.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::ParseError => "The line is not UTF-8 or not JSON.",
            Self::InvalidRequest => "Valid JSON that is not a JSON-RPC 2.0 request.",
            Self::MethodNotFound => "The method is not in the table.",
            Self::InvalidParams => {
                "`params` does not match the method's type. Unknown members are rejected."
            }
            Self::InternalError => {
                "The serve loop itself failed, for example a result could not be encoded."
            }
            Self::ProtocolMismatch => {
                "`hello` carried another protocol version. The core exits 65."
            }
            Self::HelloRequired => "A request arrived before a successful `hello`.",
            Self::NotImplemented => {
                "The method is in the table, but its work package has not landed. `data.method` names it."
            }
            Self::Internal => "The method's own work failed (I/O, database, child process).",
            Self::JobFailed => "A `job.failed` whose failure has no reason code of its own.",
            Self::EngineNotInstalled => {
                "The method needs an engine and none is installed or active."
            }
            Self::ChecksumMismatch => "An archive does not hash to the digest it must have.",
            Self::InvalidBundle => "An archive or its `BUNDLE.toml` breaks a bundle rule.",
            Self::DiskFull => "The disk filled up.",
            Self::DownloadFailed => "No published version exists, or this build cannot fetch it.",
            Self::UnsignedEngineRefused => {
                "A local bundle was requested without `APPLEJACK_ALLOW_UNSIGNED_ENGINE=1`."
            }
        }
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = i64::deserialize(deserializer)?;
        Self::from_code(code)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown error code {code}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn version_accepts_only_2_0() {
        assert_eq!(serde_json::to_value(Version).unwrap(), json!("2.0"));
        assert!(serde_json::from_value::<Version>(json!("1.0")).is_err());
        assert!(serde_json::from_value::<Version>(json!(2)).is_err());
    }

    #[test]
    fn id_accepts_number_or_string() {
        assert_eq!(
            serde_json::from_value::<Id>(json!(7)).unwrap(),
            Id::Number(7)
        );
        assert_eq!(
            serde_json::from_value::<Id>(json!("a")).unwrap(),
            Id::String("a".into())
        );
        assert!(serde_json::from_value::<Id>(json!(1.5)).is_err());
    }

    #[test]
    fn request_defaults_missing_params_to_empty_object() {
        let request: Request =
            serde_json::from_value(json!({"jsonrpc": "2.0", "id": 1, "method": "hello"})).unwrap();
        assert_eq!(request.params, json!({}));
        assert!(
            serde_json::from_value::<Request>(
                json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "extra": 1})
            )
            .is_err()
        );
    }

    #[test]
    fn response_flattens_result_or_error() {
        let ok = Response::result(Id::Number(1), json!({"a": 1}));
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"a": 1}})
        );
        let failed = Response::error(None, RpcError::new(ErrorCode::ParseError, "bad"));
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "bad"}})
        );
        let parsed: Response =
            serde_json::from_value(serde_json::to_value(&failed).unwrap()).unwrap();
        assert_eq!(parsed, failed);
    }

    #[test]
    fn error_codes_round_trip_as_integers_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in ErrorCode::ALL {
            assert!(seen.insert(code.code()), "duplicate code {}", code.code());
            let value = serde_json::to_value(code).unwrap();
            assert_eq!(value, json!(code.code()));
            assert_eq!(serde_json::from_value::<ErrorCode>(value).unwrap(), code);
        }
        assert!(serde_json::from_value::<ErrorCode>(json!(42)).is_err());
    }
}
