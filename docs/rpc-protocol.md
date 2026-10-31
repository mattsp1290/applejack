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
  without buffering it. The cap counts every byte before the `\n`, so a
  trailing `\r` counts too: 16 MiB of content plus `\r\n` is refused.
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
{"jsonrpc":"2.0","id":2,"error":{"code":1002,"message":"`app.run` is pending (work package 5)","data":{"method":"app.run"}}}
```

A success never carries `"result": null`. A method with nothing to return
answers `{}` (`EmptyResult` in `methods.rs`, mirrored in `RpcTypes.swift`);
`rpc::to_result` maps a unit value to `{}` and the loop does the same for a
handler that returns `null` directly. The Swift client decodes a present
`result` whatever its value, so a `null` would fail a typed decode loudly
instead of reading as "no result".

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

Codes are serialized as integers. The table between the markers is rendered
from `ErrorCode::ALL` in `rpc/wire.rs` by `docs_error_table_matches_the_types`
in `tests/rpc_golden.rs`, which also emits `tests/rpc_golden/error_codes.json`
(name to integer) for the Swift suite to check `RpcErrorCode` against. Add a
code to the Rust enum, rerun with `APPLEJACK_UPDATE_GOLDENS=1`, and add the
Swift case; do not edit the table by hand.

<!-- error-table:begin -->
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
| 1004 | `JobFailed` | A `job.failed` whose failure has no reason code of its own. |
| 1005 | `EngineNotInstalled` | The method needs an engine and none is installed or active. |
| 1006 | `ChecksumMismatch` | An archive does not hash to the digest it must have. |
| 1007 | `InvalidBundle` | An archive or its `BUNDLE.toml` breaks a bundle rule. |
| 1008 | `DiskFull` | The disk filled up. |
| 1009 | `DownloadFailed` | No published version exists, or this build cannot fetch it. |
| 1010 | `UnsignedEngineRefused` | A local bundle was requested without `APPLEJACK_ALLOW_UNSIGNED_ENGINE=1`. |
| 1011 | `PrefixBusy` | Another operation is using the prefix. |
<!-- error-table:end -->

A `job.failed` carries the reason code of its failure when it has one
(`ChecksumMismatch`, `InvalidBundle`, `DiskFull`, ...) and `JobFailed`
otherwise; a request refused before its job starts answers with the same
codes as an error response.

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
| `engine.list` | implemented |
| `engine.install` | implemented |
| `engine.use` | implemented |
| `engine.doctor` | implemented |
| `engine.probe_cpuid` | implemented |
| `engine.d3dmetal.import` | implemented |
| `engine.d3dmetal.remove` | implemented |
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
| `config.get` | implemented |
| `config.set` | implemented |
| `config.export` | implemented |
| `config.import` | implemented |
| `prefix.snapshot` | implemented |
| `prefix.restore` | implemented |
| `prefix.reset` | implemented |
| `session.list` | pending (work package 5) |
<!-- method-table:end -->

A pending method still validates its params. Bad params get `InvalidParams`;
good params get `NotImplemented`.

### Results

The Rust types in `crates/applejack-core/src/{engines,prefixes,game_config}.rs`
define the results, and `tests/rpc_golden/<method>.result.json` carries one
example of each, read by both `tests/rpc_golden.rs` and the Swift `GoldenTests`.

| Method | Result |
|---|---|
| `engine.list` | `{"engines": [{engine_id, version, installed, active}]}`: versions on disk and in the manifest, oldest first |
| `engine.install` | `JobStarted`; see [Jobs](#jobs) |
| `engine.use` | `{}` |
| `engine.doctor` | `{runnable, findings: [{severity, message}]}`. No engine is a finding whose last entry is the fix, not an error |
| `engine.probe_cpuid` | `{plain, advertised, changes_bits}`; also stored in `meta.cpuid_probe` |
| `engine.d3dmetal.import` | `{engine_version, overlay_dir, imported_from, gptk_version, accepted_at}` |
| `engine.d3dmetal.remove` | `{}` |
| `prefix.snapshot` | `{prefix, timestamp, dir}` |
| `prefix.restore` | The same shape, naming the safety snapshot of the replaced state, or the restored snapshot when the prefix was gone and there was nothing to replace |
| `prefix.reset` | `JobStarted`. A `job.progress` message names the safety snapshot |
| `config.get` | `GameConfig`; every member `null` for an appid with no row |
| `config.set` | `{}`. `value` is a string, a boolean, a number, an object for `extra_env`, or `null` to clear |
| `config.export` | `{path}` |
| `config.import` | `{applied, preset_appid, accepted, rejected, confirmations}` |

`engine.install` takes an optional `from`, a bundle directory or `.tar.zst` on
this Mac. It needs the core to run with `APPLEJACK_ALLOW_UNSIGNED_ENGINE=1`,
which a release build ignores; without `from` the embedded manifest decides,
and an empty one answers `DownloadFailed`, naming `--from` in a debug build.

`config.import` answers a result, not an error, when the plan needs the
launch-override confirmation: `applied` is false and `confirmations` lists what
to confirm, and the request is repeated with `accept_launch_overrides: true`.
The request's `appid` wins over the file's, which comes back as `preset_appid`.

The other engine and config methods run on the request loop, so a `hello` sent
behind one waits for it. `prefix.snapshot` and `prefix.restore` wait up to ten
seconds for a `wineserver` that outlives the prefix's last process.

One operation at a time runs on a prefix, across jobs and processes, under an
exclusive lock at `prefixes/.snapshots/.locks/<name>.lock`. The lock covers the
snapshot, restore, reset, remove, create, and exec operations, so a CLI command
beside `serve` counts. The loser does not wait: it answers `PrefixBusy`.
`prefix.reset` takes the lock before it answers, so a busy or unknown prefix is
an error response, not a job, and the job holds the lock until it ends.

`prefix.restore` also works when the prefix directory is gone, which is the way
back from a failed reset, or from a removal to the last snapshot: `prefix.remove`
takes none of its own. The method needs only the snapshot; a name with neither
a prefix nor a snapshot is an unknown prefix.

## Jobs

A long operation answers right away with `{"job_id": <u64>}` (`JobStarted`),
then emits notifications for that id:

- `job.progress` `{job_id, phase, done_bytes, total_bytes, message}`. The byte
  counts are `null` when unknown.
- Exactly one final `job.done` `{job_id}` or `job.failed`
  `{job_id, code, message}`, where `code` is an integer from the table above.

A `job.failed` for a reset that got past its safety snapshot says which
snapshot to restore, and carries the code of the underlying failure.

`engine.install` and `prefix.reset` run as jobs. Every message, from the
request loop or a job thread, goes as one whole line through one channel to
one writer thread, so a notification and a response never interleave on stdout
and a job never holds up the loop (`rpc::jobs::Notifier`). A handler gets the
notifier with each call and starts work with `Notifier::start_job`. A job that
panics ends in `job.failed` with `InternalError`.

EOF on stdin stops reading, and the core exits once running jobs have sent
their final notification. A reader that closes stdout does not change that:
lines for it are dropped, the jobs run to the end, and the core then exits with
the write error. `CoreClient.shutdown()` does not wait that long; a core
terminated during an install leaves no `<version>/`, and the next install
replaces the partial state and clears any packed `<version>.local.tar.zst`. Only
one install of an engine runs at a time, across processes
(`engines/<id>/.install.lock`); a second one ends in `job.failed` saying so.

A core terminated during `prefix.reset` can leave the prefix missing or half
built, and no notification names the safety snapshot. The newest entry of
`applejack prefix list-snapshots <name>` is that snapshot, and
`applejack prefix restore <name> <timestamp>` brings the prefix back. There is
no RPC method for the list yet. Over a half-built prefix the restore takes a
safety snapshot of it first, as any restore does. The kernel drops the lock with
the process, so a terminated core leaves nothing locked.

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

Error responses the loop produces without running a method (`ParseError`,
`InvalidRequest`, `MethodNotFound`, `InvalidParams`, `HelloRequired`,
`ProtocolMismatch`) are logged too, as `<- <method> error ...` with `?` for a
line whose method could not be read. For a `steam.login.*` method only the
code and name are logged, because an `InvalidParams` message quotes the
offending value.

## Restart policy

`CoreClient` (`apps/Applejack/Sources/ApplejackShell/CoreClient.swift`) spawns
the core, sends one `hello` per spawn, and matches responses to requests by
`id`. Every caller that arrives while the handshake is in flight waits for the
same `hello`; cancelling one of them releases that caller at once and does not
cancel the handshake. A `hello` that is not answered within ten seconds
(`handshakeTimeout`) fails every waiter with `handshakeTimedOut` and tears the
core down; a `hello` that fails for any reason other than a protocol mismatch
tears the core down the same way, and the next call respawns it.

The client observes the core's exit through its termination status, not
through EOF on stdout. After the core exits it drains stdout for 250 ms, then
stops reading and fails the requests in flight with `coreExited(status)`.
`coreExited` means the outcome is unknown: the core may have acted on the
request before it exited. Check state before retrying a non-idempotent call
such as `app.install` or `prefix.reset`. A request sent while no core is
running, or whose write failed because the core closed its stdin, fails with
`notRunning`.

Restarts are lazy: the next call respawns the core, at most three times, and
a spawn that fails outright (`launchFailed`) does not count. After the third
restart every call fails with `restartLimit`. A protocol mismatch is final:
the client does not respawn into the same mismatch.

`shutdown()` closes the core's stdin, which ends a healthy core's loop with
exit 0, terminates the core if it is still running two seconds later, and
kills it one second after that if it ignored the termination.

A response with `"id": null` (`ParseError` or `InvalidRequest` for a line the
core could not attribute) matches no request; the client logs it to stderr.
The request that caused it keeps waiting, because the client cannot tell
which one it was.

Cancelling a Swift task stops its wait. A request not yet written is never
sent; one already written still runs in the core, and its late response is
dropped. `app.cancel` (work package 5) is the method that stops work in the
core.

Reattaching running game sessions after a core restart is work package 5.

## Goldens

`crates/applejack-core/tests/rpc_golden/` holds `<method>.request.json` for
every method, plus `<method>.result.json` for methods with a result type
(`hello`, `host.doctor`), `empty.result.json` for the `{}` result, and
`error_codes.json` (error name to integer). They are generated from the Rust
types:

- `cargo test -p applejack-core --test rpc_golden` fails when a committed
  golden differs from what the types produce, when one is missing, or when
  one has no method in the table.
- `APPLEJACK_UPDATE_GOLDENS=1 cargo test -p applejack-core --test rpc_golden`
  rewrites them. Review the diff and commit it with the type change.
- The same test file deserializes every committed request into the method
  table.
- The Swift suite (`GoldenTests`) decodes every golden into the Swift mirrors
  in `RpcTypes.swift`. It checks that each request, and each typed result,
  re-encodes to the same JSON, so a field added on the Rust side fails the
  Swift suite until the mirror carries it; that `CoreClient`'s own request
  encoder writes `hello` byte-for-byte as the golden; and that `RpcErrorCode`
  has exactly the values in `error_codes.json`.

Adding a method means adding a row to `methods!` and an example to
`Method::examples` in `methods.rs`, then regenerating the goldens and the
method table above with `APPLEJACK_UPDATE_GOLDENS=1`. The same test run
fails until both match, and `CoreHandler` must answer `NotImplemented` for
exactly the methods whose status is pending.
