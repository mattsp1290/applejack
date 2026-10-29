//! JSON-RPC 2.0 over stdio, one message per line, as served by
//! `applejack serve --stdio`. The shell sends `hello` first; everything else
//! is answered through a [`Handler`]. `docs/rpc-protocol.md` is the written
//! form of this module.

pub mod jobs;
pub mod methods;
pub mod wire;

use crate::host::HostFacts;
use methods::{HelloResult, Method, MethodStatus};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use wire::{ErrorCode, Id, Request, Response, RpcError};

pub const PROTOCOL_VERSION: u32 = 1;

/// Replaces redacted values in logs.
pub const REDACTED: &str = "<redacted>";

/// Why the loop returned; `code` is the process exit status to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// The reader hit EOF; the shell closed the pipe.
    Eof,
    /// `hello` carried another protocol version; nothing was served.
    ProtocolMismatch,
}

impl ExitStatus {
    /// `0` for EOF, `EX_DATAERR` (65, sysexits(3)) for a protocol mismatch.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Eof => 0,
            Self::ProtocolMismatch => 65,
        }
    }
}

/// Answers parsed requests. The loop owns framing, `hello`, and the version
/// check; the handler sees `hello` only once those passed.
pub trait Handler {
    /// # Errors
    ///
    /// The error becomes the response's `error` member.
    fn handle(&mut self, method: &Method) -> Result<Value, RpcError>;
}

/// The real core: `hello` and `host.doctor` now, `NotImplemented` for the
/// rest until their work packages land.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreHandler;

impl Handler for CoreHandler {
    fn handle(&mut self, method: &Method) -> Result<Value, RpcError> {
        match method {
            Method::Hello(_) => to_result(&HelloResult {
                protocol_version: PROTOCOL_VERSION,
                core_version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            Method::HostDoctor(_) => to_result(&HostFacts::gather()),
            pending => Err(not_implemented(pending)),
        }
    }
}

fn to_result<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| {
        RpcError::new(
            ErrorCode::InternalError,
            format!("cannot encode result: {error}"),
        )
    })
}

#[must_use]
pub fn not_implemented(method: &Method) -> RpcError {
    let name = method.name();
    let message = match method.status() {
        MethodStatus::Pending { work_package } => {
            format!("`{name}` is pending (work package {work_package})")
        }
        MethodStatus::Implemented => format!("`{name}` is not served by this handler"),
    };
    RpcError::new(ErrorCode::NotImplemented, message).with_data(json!({"method": name}))
}

/// Replaces `steam.login.*` params and results with [`REDACTED`] before they
/// reach a log. Objects keep their keys so a log still shows the shape.
#[must_use]
pub fn redact(method: &str, value: &Value) -> Value {
    if !method.starts_with("steam.login.") {
        return value.clone();
    }
    match value {
        Value::Object(map) => map
            .keys()
            .map(|key| (key.clone(), Value::String(REDACTED.to_owned())))
            .collect(),
        _ => Value::String(REDACTED.to_owned()),
    }
}

/// Logs every request and response through [`redact`] to `sink` (stderr in
/// the CLI), then defers to `inner`. A sink write failure never fails a call.
#[derive(Debug)]
pub struct Logged<H, W> {
    inner: H,
    sink: W,
}

impl<H, W> Logged<H, W> {
    pub fn new(inner: H, sink: W) -> Self {
        Self { inner, sink }
    }

    pub fn into_parts(self) -> (H, W) {
        (self.inner, self.sink)
    }
}

impl<H: Handler, W: Write> Handler for Logged<H, W> {
    fn handle(&mut self, method: &Method) -> Result<Value, RpcError> {
        let name = method.name();
        let params = redact(name, &method.params());
        let _ = writeln!(self.sink, "-> {name} {params}");
        let outcome = self.inner.handle(method);
        let _ = match &outcome {
            Ok(result) => writeln!(self.sink, "<- {name} ok {}", redact(name, result)),
            Err(error) => writeln!(self.sink, "<- {name} error {error}"),
        };
        outcome
    }
}

/// Serves newline-delimited JSON-RPC until EOF or a protocol mismatch.
///
/// A line that is not UTF-8 or not JSON answers `ParseError` with a null id;
/// valid JSON that is not a request answers `InvalidRequest`; both continue
/// the loop. Empty lines are skipped. Every response is flushed before the
/// next line is read.
///
/// # Errors
///
/// Only I/O errors on `reader` or `writer`; protocol problems are answered
/// on the wire instead.
pub fn serve<H: Handler + ?Sized>(
    mut reader: impl BufRead,
    mut writer: impl Write,
    handler: &mut H,
) -> io::Result<ExitStatus> {
    let mut greeted = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(ExitStatus::Eof);
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            let error = RpcError::new(ErrorCode::ParseError, "line is not UTF-8");
            respond(&mut writer, &Response::error(None, error))?;
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let request = match parse_request(text) {
            Ok(request) => request,
            Err(response) => {
                respond(&mut writer, &response)?;
                continue;
            }
        };
        let (response, exit) = dispatch(request, &mut greeted, handler);
        respond(&mut writer, &response)?;
        if let Some(exit) = exit {
            return Ok(exit);
        }
    }
}

fn parse_request(text: &str) -> Result<Request, Response> {
    match serde_json::from_str::<Request>(text) {
        Ok(request) => Ok(request),
        Err(error) if error.is_data() => {
            // Valid JSON but not a request: echo the id when one is readable.
            let id = serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|value| value.get("id").cloned())
                .and_then(|id| serde_json::from_value::<Id>(id).ok());
            Err(Response::error(
                id,
                RpcError::new(ErrorCode::InvalidRequest, error.to_string()),
            ))
        }
        Err(error) => Err(Response::error(
            None,
            RpcError::new(ErrorCode::ParseError, error.to_string()),
        )),
    }
}

fn dispatch<H: Handler + ?Sized>(
    request: Request,
    greeted: &mut bool,
    handler: &mut H,
) -> (Response, Option<ExitStatus>) {
    let Request {
        id, method, params, ..
    } = request;
    let is_hello = method == "hello";
    if !*greeted && !is_hello {
        let error = RpcError::new(
            ErrorCode::HelloRequired,
            format!("send `hello` before `{method}`"),
        );
        return (Response::error(Some(id), error), None);
    }
    let method = match Method::from_request(&method, params) {
        Ok(method) => method,
        Err(error) => return (Response::error(Some(id), error), None),
    };
    if let Method::Hello(params) = &method
        && params.protocol_version != PROTOCOL_VERSION
    {
        let error = RpcError::new(
            ErrorCode::ProtocolMismatch,
            format!(
                "core speaks protocol {PROTOCOL_VERSION}, shell sent {}",
                params.protocol_version
            ),
        )
        .with_data(json!({
            "expected": PROTOCOL_VERSION,
            "received": params.protocol_version,
        }));
        return (
            Response::error(Some(id), error),
            Some(ExitStatus::ProtocolMismatch),
        );
    }
    let response = match handler.handle(&method) {
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(Some(id), error),
    };
    if is_hello && response.is_ok() {
        *greeted = true;
    }
    (response, None)
}

fn respond(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    // A response is built from the wire types; it always encodes.
    let text = serde_json::to_string(response)?;
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}
