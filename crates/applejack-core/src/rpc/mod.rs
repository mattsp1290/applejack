//! JSON-RPC 2.0 over stdio, one message per line, as served by
//! `applejack serve --stdio`. The shell sends `hello` first; everything else
//! is answered through a [`Handler`]. `docs/rpc-protocol.md` is the written
//! form of this module.

pub mod jobs;
pub mod methods;
pub mod wire;

use crate::config::DataDirError;
use crate::engines::describe_progress;
use crate::error::CoreError;
use crate::game_config::config_value_text;
use crate::host::HostFacts;
use crate::state::Core;
use jobs::Notifier;
use methods::{ConfigExportResult, EmptyResult, HelloResult, Method, MethodStatus};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::Receiver;
use std::thread;
use wire::{Body, ErrorCode, Id, Request, Response, RpcError};

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
    /// Must return promptly: the loop reads the next request only after
    /// this does. Long work goes through [`Notifier::start_job`].
    ///
    /// # Errors
    ///
    /// The error becomes the response's `error` member.
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError>;

    /// Told about an error response the loop produced without calling
    /// `handle` (a parse, envelope, method, params, or `hello` problem).
    /// `method` is the request's method name when it could be read. The
    /// default does nothing; [`Logged`] writes it to the sink.
    fn refused(&mut self, method: Option<&str>, error: &RpcError) {
        let _ = (method, error);
    }
}

/// The real core. Methods whose work package has not landed answer
/// `NotImplemented`.
#[derive(Debug)]
pub struct CoreHandler {
    /// An unusable data directory fails the methods that need one, not
    /// `hello` or `host.doctor`.
    core: Result<Core, DataDirError>,
}

impl CoreHandler {
    #[must_use]
    pub fn new(core: Core) -> Self {
        Self { core: Ok(core) }
    }

    /// The handler this process's environment describes.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            core: Core::from_env(),
        }
    }

    fn core(&mut self) -> Result<&mut Core, RpcError> {
        self.core
            .as_mut()
            .map_err(|error| CoreError::DataDir(error.clone()).into())
    }
}

impl Handler for CoreHandler {
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        match method {
            Method::Hello(_) => to_result(&HelloResult {
                protocol_version: PROTOCOL_VERSION,
                core_version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            Method::HostDoctor(_) => to_result(&HostFacts::gather()),
            Method::EngineList(_) => to_result(&self.core()?.engine_list()?),
            Method::EngineInstall(params) => {
                let install = self
                    .core()?
                    .plan_install(&params.engine_id, params.from.as_deref())?;
                to_result(&notifier.start_job(move |job| {
                    let installed = install.run(&mut |progress| {
                        let (phase, done, total, message) = describe_progress(progress);
                        job.progress(phase, done, total, message);
                    })?;
                    job.progress(
                        "installed",
                        None,
                        None,
                        format!("{} {}", installed.id, installed.version),
                    );
                    Ok(())
                })?)
            }
            Method::EngineUse(params) => {
                self.core()?
                    .engine_use(&params.engine_id, &params.version)?;
                to_result(&EmptyResult {})
            }
            Method::EngineDoctor(_) => to_result(&self.core()?.engine_doctor()?),
            Method::EngineProbeCpuid(_) => to_result(&self.core()?.engine_probe_cpuid()?),
            Method::EngineD3dmetalImport(params) => to_result(
                &self
                    .core()?
                    .engine_d3dmetal_import(&params.path, params.license_acknowledged)?,
            ),
            Method::EngineD3dmetalRemove(_) => {
                self.core()?.engine_d3dmetal_remove()?;
                to_result(&EmptyResult {})
            }
            Method::ConfigGet(params) => to_result(&self.core()?.config_get(params.appid)?),
            Method::ConfigSet(params) => {
                let value = config_value_text(&params.value);
                self.core()?.config_set(params.appid, &params.key, &value)?;
                to_result(&EmptyResult {})
            }
            Method::ConfigExport(params) => {
                self.core()?.config_export(params.appid, &params.path)?;
                to_result(&ConfigExportResult {
                    path: params.path.clone(),
                })
            }
            Method::ConfigImport(params) => to_result(&self.core()?.config_import(
                params.appid,
                &params.path,
                params.accept_launch_overrides,
            )?),
            Method::PrefixSnapshot(params) => {
                to_result(&self.core()?.prefix_snapshot(&params.prefix)?)
            }
            Method::PrefixRestore(params) => to_result(
                &self
                    .core()?
                    .prefix_restore(&params.prefix, &params.timestamp)?,
            ),
            Method::PrefixReset(params) => {
                let core = self.core()?;
                let claim = core.claim_prefix(&params.prefix)?;
                let mut core = core.detached();
                let keep = params.keep_user_data;
                to_result(&notifier.start_job(move |job| {
                    let name = claim.name();
                    job.progress("reset", None, None, format!("rebuilding prefix {name}"));
                    let safety = core.prefix_reset_claimed(&claim, keep)?;
                    job.progress(
                        "reset",
                        None,
                        None,
                        format!("safety snapshot {}", safety.timestamp),
                    );
                    Ok(())
                })?)
            }
            pending => Err(not_implemented(pending)),
        }
    }
}

/// Encodes a handler's result. A value that encodes as `null` (unit, `()`)
/// becomes `{}` ([`methods::EmptyResult`]): a success never carries a null
/// result, so the shell can decode every result into a typed value.
///
/// # Errors
///
/// `InternalError` when the value cannot be encoded.
pub fn to_result<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value)
        .map(|value| if value.is_null() { json!({}) } else { value })
        .map_err(|error| {
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

/// Whether a method's params, results, and error details stay out of logs.
#[must_use]
pub fn is_redacted(method: &str) -> bool {
    method.starts_with("steam.login.")
}

/// Replaces `steam.login.*` params and results with [`REDACTED`] before they
/// reach a log. Objects keep their keys so a log still shows the shape.
#[must_use]
pub fn redact(method: &str, value: &Value) -> Value {
    if !is_redacted(method) {
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
    fn handle(&mut self, method: &Method, notifier: &Notifier) -> Result<Value, RpcError> {
        let name = method.name();
        let params = redact(name, &method.params());
        let _ = writeln!(self.sink, "-> {name} {params}");
        let outcome = self.inner.handle(method, notifier);
        let _ = match &outcome {
            Ok(result) => writeln!(self.sink, "<- {name} ok {}", redact(name, result)),
            // A login error's message or data may echo the account; only the
            // code is safe to log.
            Err(error) if is_redacted(name) => writeln!(
                self.sink,
                "<- {name} error {} ({})",
                error.code.name(),
                error.code.code()
            ),
            Err(error) => writeln!(self.sink, "<- {name} error {error}"),
        };
        outcome
    }

    fn refused(&mut self, method: Option<&str>, error: &RpcError) {
        let name = method.unwrap_or("?");
        // An `InvalidParams` message quotes the offending value, so a login
        // request is logged by code only, like a login error.
        let _ = if is_redacted(name) {
            writeln!(
                self.sink,
                "<- {name} error {} ({})",
                error.code.name(),
                error.code.code()
            )
        } else {
            writeln!(self.sink, "<- {name} error {error}")
        };
        self.inner.refused(method, error);
    }
}

/// The longest request line the loop reads into memory. Longer lines are
/// answered with `InvalidRequest` and skipped.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Serves newline-delimited JSON-RPC until EOF or a protocol mismatch.
///
/// A line that is not UTF-8 or not JSON answers `ParseError` with a null id;
/// valid JSON that is not a request (including a non-object or a line
/// longer than [`MAX_LINE_BYTES`]) answers `InvalidRequest`; both continue
/// the loop. Empty lines are skipped. Every message is flushed as one line
/// by a writer thread, in the order queued, so a job's notifications never
/// break a line. Before returning, the loop waits for running jobs to send
/// their final notification, also when `writer` has failed: the caller's
/// exit would kill them.
///
/// # Errors
///
/// Only I/O errors on `reader` or `writer`; protocol problems are answered
/// on the wire instead.
pub fn serve<H: Handler + ?Sized>(
    reader: impl BufRead,
    writer: impl Write + Send,
    handler: &mut H,
) -> io::Result<ExitStatus> {
    let (notifier, lines) = Notifier::channel();
    thread::scope(|scope| {
        let output = scope.spawn(move || write_lines(writer, &lines));
        let outcome = read_requests(reader, &notifier, handler);
        // The writer ends when the last sender does: this one, then the
        // clones held by job threads.
        drop(notifier);
        let written = output
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the writer thread panicked")));
        written.and(outcome)
    })
}

/// Runs until the last sender drops, whatever the writer does: `serve` waits
/// for jobs by joining this thread. Lines after a failed write are dropped.
fn write_lines(mut writer: impl Write, lines: &Receiver<String>) -> io::Result<()> {
    let mut outcome = Ok(());
    for mut line in lines {
        if outcome.is_ok() {
            line.push('\n');
            outcome = writer
                .write_all(line.as_bytes())
                .and_then(|()| writer.flush());
        }
    }
    outcome
}

fn respond(notifier: &Notifier, response: &Response) -> io::Result<()> {
    if notifier.respond(response) {
        Ok(())
    } else {
        Err(io::ErrorKind::BrokenPipe.into())
    }
}

fn read_requests<H: Handler + ?Sized>(
    mut reader: impl BufRead,
    notifier: &Notifier,
    handler: &mut H,
) -> io::Result<ExitStatus> {
    let mut greeted = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        // One byte past the cap tells a line that is too long from one that
        // is exactly at it.
        let limit = u64::try_from(MAX_LINE_BYTES + 1).unwrap_or(u64::MAX);
        if reader.by_ref().take(limit).read_until(b'\n', &mut line)? == 0 {
            return Ok(ExitStatus::Eof);
        }
        if line.len() > MAX_LINE_BYTES && !line.ends_with(b"\n") {
            line.clear();
            skip_line(&mut reader)?;
            let error = RpcError::new(
                ErrorCode::InvalidRequest,
                format!("request line exceeds {MAX_LINE_BYTES} bytes"),
            );
            handler.refused(None, &error);
            respond(notifier, &Response::error(None, error))?;
            continue;
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            let error = RpcError::new(ErrorCode::ParseError, "line is not UTF-8");
            handler.refused(None, &error);
            respond(notifier, &Response::error(None, error))?;
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let request = match parse_request(text) {
            Ok(request) => request,
            Err((method, response)) => {
                if let Body::Error(error) = &response.body {
                    handler.refused(method.as_deref(), error);
                }
                respond(notifier, &response)?;
                continue;
            }
        };
        let (response, exit) = dispatch(request, &mut greeted, handler, notifier);
        respond(notifier, &response)?;
        if let Some(exit) = exit {
            return Ok(exit);
        }
    }
}

/// Consumes the reader up to and including the next newline (or EOF) without
/// buffering the skipped bytes.
fn skip_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let (used, done) = {
            let available = reader.fill_buf()?;
            match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), available.is_empty()),
            }
        };
        reader.consume(used);
        if done {
            return Ok(());
        }
    }
}

/// On failure, the method name when the line carried one (for the log) and
/// the error response.
fn parse_request(text: &str) -> Result<Request, (Option<String>, Response)> {
    let value = serde_json::from_str::<Value>(text).map_err(|error| {
        let response = Response::error(
            None,
            RpcError::new(ErrorCode::ParseError, error.to_string()),
        );
        (None, response)
    })?;
    // Serde would also read a request out of an array; JSON-RPC does not.
    let Value::Object(object) = value else {
        let response = Response::error(
            None,
            RpcError::new(ErrorCode::InvalidRequest, "request must be a JSON object"),
        );
        return Err((None, response));
    };
    // Valid JSON but not a request: echo the id when one is readable.
    let id = object
        .get("id")
        .and_then(|id| serde_json::from_value::<Id>(id.clone()).ok());
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    serde_json::from_value::<Request>(Value::Object(object)).map_err(|error| {
        let response = Response::error(
            id,
            RpcError::new(ErrorCode::InvalidRequest, error.to_string()),
        );
        (method, response)
    })
}

fn dispatch<H: Handler + ?Sized>(
    request: Request,
    greeted: &mut bool,
    handler: &mut H,
    notifier: &Notifier,
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
        handler.refused(Some(&method), &error);
        return (Response::error(Some(id), error), None);
    }
    let method = match Method::from_request(&method, params) {
        Ok(method) => method,
        Err(error) => {
            handler.refused(Some(&method), &error);
            return (Response::error(Some(id), error), None);
        }
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
        handler.refused(Some(method.name()), &error);
        return (
            Response::error(Some(id), error),
            Some(ExitStatus::ProtocolMismatch),
        );
    }
    let response = match handler.handle(&method, notifier) {
        // Belt and braces for a handler that bypasses `to_result`.
        Ok(result) if result.is_null() => Response::result(id, json!({})),
        Ok(result) => Response::result(id, result),
        Err(error) => Response::error(Some(id), error),
    };
    if is_hello && response.is_ok() {
        *greeted = true;
    }
    (response, None)
}
