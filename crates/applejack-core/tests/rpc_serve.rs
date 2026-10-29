//! The serve loop over in-memory pipes.

use applejack_core::rpc::methods::{LoginMode, Method, MethodStatus};
use applejack_core::rpc::wire::{Body, ErrorCode, Id, Response, RpcError};
use applejack_core::rpc::{self, CoreHandler, ExitStatus, Handler, Logged, PROTOCOL_VERSION};
use serde_json::{Value, json};

const HELLO: &str = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1}}"#;
const DOCTOR: &str = r#"{"jsonrpc":"2.0","id":2,"method":"host.doctor","params":{}}"#;
const MISMATCH: &str =
    r#"{"jsonrpc":"2.0","id":9,"method":"hello","params":{"protocol_version":99}}"#;

/// Answers every method with its name so tests can see what was dispatched.
/// `steam.login.complete` fails the way a real login handler might: with the
/// account and the guard code in the error.
#[derive(Default)]
struct Echo {
    seen: Vec<String>,
    account: Option<String>,
}

impl Handler for Echo {
    fn handle(&mut self, method: &Method) -> Result<Value, RpcError> {
        self.seen.push(method.name().to_owned());
        match method {
            Method::SteamLoginStart(params) => {
                if let LoginMode::Credentials { account_name, .. } = &params.mode {
                    self.account = Some(account_name.clone());
                }
            }
            Method::SteamLoginComplete(params) => {
                let account = self.account.as_deref().unwrap_or("anonymous");
                let code = params.steam_guard_code.as_deref().unwrap_or("");
                return Err(RpcError::new(
                    ErrorCode::Internal,
                    format!("Steam Guard code {code} rejected for account {account}"),
                )
                .with_data(method.params()));
            }
            _ => {}
        }
        Ok(json!({"method": method.name(), "params": method.params()}))
    }
}

fn run<H: Handler>(input: &str, handler: &mut H) -> (Vec<Response>, ExitStatus) {
    let mut output = Vec::new();
    let status = rpc::serve(input.as_bytes(), &mut output, handler).expect("in-memory I/O");
    let text = String::from_utf8(output).expect("responses are UTF-8");
    let responses = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    (responses, status)
}

fn error_of(response: &Response) -> &RpcError {
    match &response.body {
        Body::Error(error) => error,
        Body::Result(value) => panic!("expected an error, got {value}"),
    }
}

fn result_of(response: &Response) -> &Value {
    match &response.body {
        Body::Result(value) => value,
        Body::Error(error) => panic!("expected a result, got {error}"),
    }
}

#[test]
fn hello_then_host_doctor_round_trip() {
    let input = format!("{HELLO}\n{DOCTOR}\n");
    let (responses, status) = run(&input, &mut CoreHandler);
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(status.code(), 0);
    assert_eq!(responses.len(), 2);

    assert_eq!(responses[0].id, Some(Id::Number(1)));
    let hello = result_of(&responses[0]);
    assert_eq!(hello["protocol_version"], json!(PROTOCOL_VERSION));
    assert_eq!(hello["core_version"], json!(env!("CARGO_PKG_VERSION")));

    assert_eq!(responses[1].id, Some(Id::Number(2)));
    let facts = result_of(&responses[1]);
    assert!(facts["macos_version"].is_string());
    assert_eq!(facts["screen_recording"], json!("unknown"));
}

#[test]
fn request_before_hello_is_refused_and_hello_still_works() {
    let input = format!("{DOCTOR}\n{HELLO}\n{DOCTOR}\n");
    let mut handler = Echo::default();
    let (responses, status) = run(&input, &mut handler);
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses.len(), 3);
    let refused = error_of(&responses[0]);
    assert_eq!(refused.code, ErrorCode::HelloRequired);
    assert_eq!(responses[0].id, Some(Id::Number(2)));
    assert!(responses[1].is_ok());
    assert!(responses[2].is_ok());
    assert_eq!(handler.seen, ["hello", "host.doctor"]);
}

#[test]
fn protocol_mismatch_answers_and_ends_the_loop_non_zero() {
    let input = format!("{MISMATCH}\n{DOCTOR}\n");
    let mut handler = Echo::default();
    let (responses, status) = run(&input, &mut handler);
    assert_eq!(status, ExitStatus::ProtocolMismatch);
    assert_ne!(status.code(), 0);
    assert_eq!(responses.len(), 1, "nothing after the mismatch is answered");
    let error = error_of(&responses[0]);
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert_eq!(error.data, Some(json!({"expected": 1, "received": 99})));
    assert!(
        handler.seen.is_empty(),
        "the handler never sees a bad hello"
    );
}

#[test]
fn malformed_line_gets_parse_error_with_null_id_and_loop_continues() {
    let input = format!("{{not json\n{HELLO}\n");
    let (responses, status) = run(&input, &mut Echo::default());
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0].id, None);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::ParseError);
    assert!(responses[1].is_ok());
}

#[test]
fn non_utf8_line_gets_parse_error() {
    let mut input = b"\xff\xfe\n".to_vec();
    input.extend_from_slice(HELLO.as_bytes());
    input.push(b'\n');
    let mut output = Vec::new();
    let status = rpc::serve(input.as_slice(), &mut output, &mut Echo::default()).unwrap();
    assert_eq!(status, ExitStatus::Eof);
    let text = String::from_utf8(output).unwrap();
    let responses: Vec<Response> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::ParseError);
    assert_eq!(responses[0].id, None);
}

#[test]
fn valid_json_that_is_not_a_request_gets_invalid_request_with_its_id() {
    let input = format!(
        "{}\n{}\n{HELLO}\n",
        r#"{"jsonrpc":"2.0","id":"abc","params":{}}"#,
        r#"{"jsonrpc":"1.0","id":3,"method":"hello","params":{}}"#
    );
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(responses.len(), 3);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::InvalidRequest);
    assert_eq!(responses[0].id, Some(Id::String("abc".into())));
    assert_eq!(error_of(&responses[1]).code, ErrorCode::InvalidRequest);
    assert_eq!(responses[1].id, Some(Id::Number(3)));
    assert!(responses[2].is_ok());
}

#[test]
fn non_object_json_gets_invalid_request_with_null_id() {
    // Serde would read a request out of the array form; JSON-RPC does not.
    let input = format!(
        "{}\n42\n\"hello\"\n{HELLO}\n",
        r#"["2.0",7,"host.doctor",{}]"#
    );
    let mut handler = Echo::default();
    let (responses, _) = run(&input, &mut handler);
    assert_eq!(responses.len(), 4);
    for response in &responses[..3] {
        assert_eq!(error_of(response).code, ErrorCode::InvalidRequest);
        assert_eq!(response.id, None);
    }
    assert!(responses[3].is_ok());
    assert_eq!(handler.seen, ["hello"], "the array form was not dispatched");
}

#[test]
fn request_without_id_gets_invalid_request_with_null_id() {
    let input = format!(
        "{HELLO}\n{}\n{DOCTOR}\n",
        r#"{"jsonrpc":"2.0","method":"host.doctor","params":{}}"#
    );
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(responses.len(), 3, "a shell-sent notification is answered");
    assert_eq!(error_of(&responses[1]).code, ErrorCode::InvalidRequest);
    assert_eq!(responses[1].id, None);
    assert!(responses[2].is_ok());
}

fn junk_line(length: usize) -> String {
    "x".repeat(length)
}

#[test]
fn overlong_line_gets_invalid_request_and_the_loop_continues() {
    let input = format!("{}\n{HELLO}\n", junk_line(rpc::MAX_LINE_BYTES + 1));
    let (responses, status) = run(&input, &mut Echo::default());
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses.len(), 2);
    let error = error_of(&responses[0]);
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(error.message.contains("exceeds"), "{}", error.message);
    assert_eq!(responses[0].id, None);
    assert!(
        responses[1].is_ok(),
        "the rest of the long line was skipped"
    );
}

#[test]
fn line_at_the_cap_is_parsed_rather_than_refused() {
    let input = format!("{}\n{HELLO}\n", junk_line(rpc::MAX_LINE_BYTES));
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(responses.len(), 2);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::ParseError);
    assert!(responses[1].is_ok());
}

#[test]
fn overlong_line_without_newline_at_eof_is_refused_then_eof() {
    let input = junk_line(rpc::MAX_LINE_BYTES + 1);
    let (responses, status) = run(&input, &mut Echo::default());
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses.len(), 1);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::InvalidRequest);
}

#[test]
fn empty_lines_are_skipped() {
    let input = format!("\n{HELLO}\n   \n{DOCTOR}\n");
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(responses.len(), 2);
}

#[test]
fn unknown_method_gets_method_not_found() {
    let input = format!(
        "{HELLO}\n{}\n",
        r#"{"jsonrpc":"2.0","id":5,"method":"nope","params":{}}"#
    );
    let (responses, _) = run(&input, &mut Echo::default());
    let error = error_of(&responses[1]);
    assert_eq!(error.code, ErrorCode::MethodNotFound);
    assert_eq!(responses[1].id, Some(Id::Number(5)));
}

#[test]
fn bad_params_get_invalid_params() {
    let input = format!(
        "{HELLO}\n{}\n",
        r#"{"jsonrpc":"2.0","id":5,"method":"engine.install","params":{"engine":"x"}}"#
    );
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(error_of(&responses[1]).code, ErrorCode::InvalidParams);
}

#[test]
fn pending_methods_answer_not_implemented_naming_the_work_package() {
    let mut input = format!("{HELLO}\n");
    let pending: Vec<Method> = Method::examples()
        .into_iter()
        .filter(|method| matches!(method.status(), MethodStatus::Pending { .. }))
        .collect();
    assert!(pending.len() > 1, "the table has pending methods");
    for (index, method) in pending.iter().enumerate() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": index + 10,
            "method": method.name(),
            "params": method.params(),
        });
        input.push_str(&request.to_string());
        input.push('\n');
    }
    let (responses, status) = run(&input, &mut CoreHandler);
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses.len(), pending.len() + 1);
    for (method, response) in pending.iter().zip(&responses[1..]) {
        let error = error_of(response);
        assert_eq!(error.code, ErrorCode::NotImplemented, "{}", method.name());
        assert!(
            error.message.contains("work package"),
            "{}: {}",
            method.name(),
            error.message
        );
        assert_eq!(error.data, Some(json!({"method": method.name()})));
    }
}

#[test]
fn string_ids_are_echoed() {
    let input = r#"{"jsonrpc":"2.0","id":"req-1","method":"hello","params":{"protocol_version":1}}
"#;
    let (responses, _) = run(input, &mut Echo::default());
    assert_eq!(responses[0].id, Some(Id::String("req-1".into())));
}

/// `status()` and `CoreHandler` must agree on which methods are served, so
/// a work package that lands a method flips exactly one row in the table.
#[test]
fn core_handler_answers_not_implemented_exactly_for_pending_methods() {
    for method in Method::examples() {
        let outcome = CoreHandler.handle(&method);
        let pending = matches!(method.status(), MethodStatus::Pending { .. });
        assert_eq!(
            matches!(&outcome, Err(error) if error.code == ErrorCode::NotImplemented),
            pending,
            "{}: status and handler disagree",
            method.name()
        );
    }
}

#[test]
fn hello_may_be_repeated() {
    let input = format!("{HELLO}\n{HELLO}\n{DOCTOR}\n");
    let (responses, _) = run(&input, &mut Echo::default());
    assert_eq!(responses.len(), 3);
    assert!(responses.iter().all(Response::is_ok));
}

#[test]
fn mismatched_hello_after_a_good_one_still_exits_65() {
    let input = format!("{HELLO}\n{MISMATCH}\n{DOCTOR}\n");
    let (responses, status) = run(&input, &mut Echo::default());
    assert_eq!(status, ExitStatus::ProtocolMismatch);
    assert_eq!(status.code(), 65);
    assert_eq!(responses.len(), 2, "nothing after the mismatch is answered");
    assert!(responses[0].is_ok());
    assert_eq!(error_of(&responses[1]).code, ErrorCode::ProtocolMismatch);
}

#[test]
fn eof_without_a_good_hello_exits_0() {
    let (responses, status) = run(&format!("{DOCTOR}\n"), &mut Echo::default());
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(status.code(), 0);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::HelloRequired);

    let (responses, status) = run("", &mut Echo::default());
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(responses, Vec::<Response>::new());
}

#[test]
fn redact_hides_steam_login_values_and_nothing_else() {
    let params =
        json!({"mode": {"kind": "credentials", "account_name": "me", "password": "hunter2"}});
    let redacted = rpc::redact("steam.login.start", &params);
    assert_eq!(redacted, json!({"mode": "<redacted>"}));
    assert_eq!(
        rpc::redact("steam.login.complete", &json!("token")),
        json!("<redacted>")
    );
    assert_eq!(rpc::redact("steam.logout", &params), params);
    assert_eq!(
        rpc::redact("host.doctor", &json!({"a": 1})),
        json!({"a": 1})
    );
}

#[test]
fn logged_handler_writes_redacted_lines_to_the_sink_only() {
    let login = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "steam.login.start",
        "params": {"mode": {"kind": "credentials", "account_name": "swordfish", "password": "hunter2"}},
    });
    let complete = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "steam.login.complete",
        "params": {"session_id": "sess-1", "steam_guard_code": "ABCDE"},
    });
    let input = format!("{HELLO}\n{login}\n{complete}\n{DOCTOR}\n");
    let mut handler = Logged::new(Echo::default(), Vec::new());
    let mut output = Vec::new();
    rpc::serve(input.as_bytes(), &mut output, &mut handler).unwrap();
    let (echo, sink) = handler.into_parts();
    let log = String::from_utf8(sink).unwrap();
    assert_eq!(
        echo.seen,
        [
            "hello",
            "steam.login.start",
            "steam.login.complete",
            "host.doctor"
        ]
    );
    for secret in ["hunter2", "swordfish", "ABCDE", "sess-1"] {
        assert!(
            !log.contains(secret),
            "`{secret}` leaked into the log:\n{log}"
        );
    }
    assert!(
        log.contains("-> steam.login.start {\"mode\":\"<redacted>\"}"),
        "{log}"
    );
    assert!(
        log.contains(
            "<- steam.login.start ok {\"method\":\"<redacted>\",\"params\":\"<redacted>\"}"
        ),
        "{log}"
    );
    // A login error logs its code only: the message and data may echo the
    // account.
    assert!(
        log.contains("<- steam.login.complete error Internal (1003)\n"),
        "{log}"
    );
    assert!(log.contains("-> host.doctor {}"), "{log}");
    assert!(log.contains("-> hello {\"protocol_version\":1}"), "{log}");

    // The echo handler returned the password in its result and the account in
    // its error; the wire carries them (the shell asked), the log does not.
    let wire = String::from_utf8(output).unwrap();
    assert!(wire.contains("hunter2"));
    assert!(wire.contains("rejected for account swordfish"));
    assert_eq!(wire.lines().count(), 4);
}

#[test]
fn handler_errors_become_error_responses() {
    struct Failing;
    impl Handler for Failing {
        fn handle(&mut self, _: &Method) -> Result<Value, RpcError> {
            Err(RpcError::new(ErrorCode::Internal, "boom"))
        }
    }
    let input = format!("{HELLO}\n{DOCTOR}\n");
    let (responses, status) = run(&input, &mut Failing);
    assert_eq!(status, ExitStatus::Eof);
    assert_eq!(error_of(&responses[0]).code, ErrorCode::Internal);
    // A failed hello does not count as greeting.
    assert_eq!(error_of(&responses[1]).code, ErrorCode::HelloRequired);
}
