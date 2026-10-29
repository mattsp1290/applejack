# Core RPC protocol

The SwiftUI shell talks to the Rust core (`applejack serve --stdio`) over
JSON-RPC 2.0. The protocol is internal to applejack: shell and core ship
together, and there is no compatibility promise across versions. The Rust
types in `crates/applejack-core/src/rpc/` are the source of truth; this page
describes them.

## Framing

- One JSON message per line, UTF-8, terminated by `\n`. Messages never
  contain a raw newline.
- The shell writes requests to the core's stdin. The core writes responses
  and notifications to stdout. Nothing else is written to stdout while the
  core is serving.
- Blank lines are ignored.
- A line that is not UTF-8 or not JSON is answered with `ParseError` and
  `"id": null`. A JSON value that is not a valid request (not an object,
  wrong `jsonrpc`, missing `method`, unknown members) is answered with
  `InvalidRequest` and the line's `id` when it can be read, `null` otherwise.
  In both cases the core keeps serving.
- A request line longer than 16 MiB (`rpc::MAX_LINE_BYTES`) is answered with
  `InvalidRequest` and `"id": null`. The core discards the rest of the line
  without buffering it.
- Notifications are core-to-shell only. The shell must not send one: a line
  without an `id` is answered with `InvalidRequest` and `"id": null`, which
  JSON-RPC forbids for a notification but which keeps the loop simple.
- A notification may arrive between a request and its response. The shell
  matches responses by `id`, never by order.
- EOF on stdin ends the loop. The core exits 0 whether or not `hello` was
  ever sent; an empty session is not an error.

Request:

```json
{"jsonrpc":"2.0","id":1,"method":"hello","params":{"protocol_version":1}}
```

`id` is a number or a string. A missing `params` member, or `null`, means
`{}`.

Response, success or failure:

```json
{"jsonrpc":"2.0","id":1,"result":{"protocol_version":1,"core_version":"0.1.0"}}
{"jsonrpc":"2.0","id":2,"error":{"code":1002,"message":"`engine.list` is pending (work package 2)","data":{"method":"engine.list"}}}
```

Notification (core to shell only, no `id`, no reply):

```json
{"jsonrpc":"2.0","method":"job.progress","params":{"job_id":7,"phase":"download","done_bytes":1024,"total_bytes":4096,"message":""}}
```

## `hello`

The first request must be `hello` with `{"protocol_version": <u32>}`. The
current version is `1` (`PROTOCOL_VERSION`).

- If the version matches, the result is
  `{"protocol_version": 1, "core_version": "<crate version>"}`.
- If it differs, the core answers `ProtocolMismatch` with
  `data: {"expected": 1, "received": <n>}` and exits with status 65
  (`EX_DATAERR`) without reading further.
- Any other request before a successful `hello` is answered with
  `HelloRequired` and does not run.

`hello` may be sent again later; it is answered the same way, and a repeated
`hello` with another version exits 65 even after a good one.

## Error codes

Codes are serialized as integers.

| Code | Name | Meaning |
|---:|---|---|
| -32700 | `ParseError` | The line is not UTF-8 or not JSON. |
| -32600 | `InvalidRequest` | Valid JSON that is not a JSON-RPC 2.0 request. |
| -32601 | `MethodNotFound` | The method is not in the table. |
| -32602 | `InvalidParams` | `params` does not match the method's type. Unknown members are rejected. |
| -32603 | `InternalError` | The serve loop itself failed, for example a result could not be encoded. |
| 1000 | `ProtocolMismatch` | `hello` carried another protocol version. The core exits 65. |
| 1001 | `HelloRequired` | A request arrived before a successful `hello`. |
| 1002 | `NotImplemented` | The method is in the table, but its work package has not landed. `data.method` names it. |
| 1003 | `Internal` | The method's own work failed (I/O, database, child process). |
| 1004 | `JobFailed` | The typed code carried by a `job.failed` notification. |

## Methods

Parameter types are in `crates/applejack-core/src/rpc/methods.rs`. Each
method has an example request in `crates/applejack-core/tests/rpc_golden/`.
`host.doctor` returns `HostFacts`, the same shape as `applejack doctor
--json`.

The table between the markers is rendered from the method table by
`docs_method_table_matches_the_types` in `tests/rpc_golden.rs`; edit
`methods.rs` and rerun with `APPLEJACK_UPDATE_GOLDENS=1` instead of editing it
by hand.

<!-- method-table:begin -->
| Method | Status |
|---|---|
| `hello` | implemented |
| `host.doctor` | implemented |
| `engine.list` | pending (work package 2) |
| `engine.install` | pending (work package 2) |
| `engine.use` | pending (work package 2) |
| `engine.doctor` | pending (work package 2) |
| `engine.probe_cpuid` | pending (work package 2) |
| `engine.d3dmetal.import` | pending (work package 2) |
| `engine.d3dmetal.remove` | pending (work package 2) |
| `steam.login.start` | pending (work package 4) |
| `steam.login.complete` | pending (work package 4) |
| `steam.logout` | pending (work package 4) |
| `steam.library` | pending (work package 4) |
| `app.install` | pending (work package 4) |
| `app.run` | pending (work package 5) |
| `app.cancel` | pending (work package 5) |
| `runtime.steam.install` | pending (work package 5) |
| `runtime.steam.update` | pending (work package 5) |
| `runtime.steam.repair` | pending (work package 5) |
| `runtime.steam.stop` | pending (work package 5) |
| `config.get` | pending (work package 6a) |
| `config.set` | pending (work package 6a) |
| `config.export` | pending (work package 6a) |
| `config.import` | pending (work package 6a) |
| `prefix.snapshot` | pending (work package 2) |
| `prefix.restore` | pending (work package 2) |
| `prefix.reset` | pending (work package 2) |
| `session.list` | pending (work package 5) |
<!-- method-table:end -->

A pending method still validates its params. Bad params get `InvalidParams`;
good params get `NotImplemented`.

## Jobs

A long operation answers right away with `{"job_id": <u64>}` (`JobStarted`),
then emits notifications for that id:

- `job.progress` `{job_id, phase, done_bytes, total_bytes, message}`. The byte
  counts are `null` when unknown.
- Exactly one final `job.done` `{job_id}` or `job.failed`
  `{job_id, code, message}`, where `code` is an integer from the table above.

No method runs as a job yet. The first job methods (`engine.install`,
`app.install`) land with their work packages. When they do, job threads and
the request loop must write through one shared, locked writer so a
notification and a response never interleave on stdout; the handler API for
emitting notifications is that unit's to define.

Children the core spawns (wine, wineserver, Steam) must not inherit the
core's stdout: give them `Stdio::null()` or a pipe. An inherited stdout would
let a child write into the protocol stream and would keep the shell's pipe
open after the core exits.

## Logging and redaction

`applejack serve --stdio --log` writes one line per request and per response
to stderr. Without `--log` the core writes nothing to stderr while serving.
Log lines never go to stdout.

Before anything is logged, `rpc::redact` replaces every param and every result
of a `steam.login.*` method with `"<redacted>"`. Object keys are kept, so the
log still shows the shape. Values on the wire itself are not redacted.

## Restart policy

`CoreClient` (`apps/Applejack/Sources/ApplejackShell/CoreClient.swift`) spawns
the core, sends one `hello` per spawn, and matches responses to requests by
`id`. Every caller that arrives while the handshake is in flight awaits the
same `hello`; cancelling one of them does not cancel the handshake. A `hello`
that fails for any reason other than a protocol mismatch tears the core down.

The client observes the core's exit through its termination status, not
through EOF on stdout. After the core exits it drains stdout for 250 ms, then
stops reading and fails the requests in flight with `coreExited(status)`.
Restarts are lazy: the next call respawns the core, at most three times, and
a spawn that fails outright (`launchFailed`) does not count. After the third
restart every call fails with `restartLimit`. A protocol mismatch is final:
the client does not respawn into the same mismatch.

`shutdown()` closes the core's stdin, which ends a healthy core's loop with
exit 0, and terminates the core if it is still running two seconds later.

A response with `"id": null` (`ParseError` or `InvalidRequest` for a line the
core could not attribute) matches no request; the client logs it to stderr.
The request that caused it keeps waiting, because the client cannot tell
which one it was.

Cancelling a Swift task abandons its request. Nothing is sent to the core, and
a late response is dropped. `app.cancel` (work package 5) is the method that
stops work in the core.

Reattaching running game sessions after a core restart is work package 5.

## Goldens

`crates/applejack-core/tests/rpc_golden/` holds `<method>.request.json` for
every method, plus `<method>.result.json` for methods with a result type
(`hello`, `host.doctor`). They are generated from the Rust types:

- `cargo test -p applejack-core --test rpc_golden` fails when a committed
  golden differs from what the types produce, when one is missing, or when
  one has no method in the table.
- `APPLEJACK_UPDATE_GOLDENS=1 cargo test -p applejack-core --test rpc_golden`
  rewrites them. Review the diff and commit it with the type change.
- The same test file deserializes every committed request into the method
  table.
- The Swift suite (`GoldenTests`) decodes every golden into the Swift mirrors
  in `RpcTypes.swift`. It also checks that each request re-encodes to the same
  JSON.

Adding a method means adding a row to `methods!` and an example to
`Method::examples` in `methods.rs`, then regenerating the goldens and the
method table above with `APPLEJACK_UPDATE_GOLDENS=1`. The same test run
fails until both match, and `CoreHandler` must answer `NotImplemented` for
exactly the methods whose status is pending.
