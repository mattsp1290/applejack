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
{"jsonrpc":"2.0","id":2,"error":{"code":1002,"message":"`runtime.steam.stop` is pending (work package 5)","data":{"method":"runtime.steam.stop"}}}
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
| 1012 | `TitleNotInstalled` | `app.run` for a title with no `apps` row or nothing in the library. |
| 1013 | `DownloadInProgress` | `app.run` for a title with a download record, or `app.install` without `update` for a stopped update. |
| 1014 | `AlreadyRunning` | `app.run` for a title with an open session. |
| 1015 | `ClientRuntimeUnavailable` | `app.run` for a title that launches through the Steam client while the runtime is absent (`runtime.steam.install`) or needs a sign-in inside the client window. |
| 1016 | `NoLaunchExe` | `app.run` for a title with no launch executable, or one outside its install directory. |
| 1017 | `NotRunning` | `app.cancel` for a title with no open session. |
| 1018 | `TitleExitedEarly` | A direct launch ended within five seconds; the title may require the Steam client. |
| 1019 | `PrefixUnusable` | A prefix cannot be used for the operation: it is stale, broken, or a `wineserver` will not leave it. |
| 1020 | `SteamLoginRequired` | No usable sign-in: no account, no stored token, or Steam refused the token. The CLI exits 3. |
| 1021 | `SteamLoginFailed` | A login step failed and the login is over; begin again. |
| 1022 | `SteamGuardRejected` | The Steam Guard code was wrong. The login stays open for another code. |
| 1023 | `SteamRateLimited` | Steam throttled the request. The message says when to retry; nothing retries on its own. |
| 1024 | `SteamUnavailable` | Steam could not be reached or denied one call on a healthy session. |
| 1025 | `AppNotOwned` | `app.install` for a title this account does not own. |
| 1026 | `TitleNotInstallable` | The title has no Windows depot, none on the branch, or none this account licenses. |
| 1027 | `LaunchExeAmbiguous` | No launch entry can be chosen: several fit and none outranks the rest, or the only one left stands behind a `default` entry that could have run on Windows. `launch_exe_override` is the way out. |
| 1028 | `InstallCancelled` | The install stopped on request. Its download record stays, and the next install resumes. |
| 1029 | `LockHeld` | Another process holds this title's install lock or the Steam session. |
| 1030 | `AlreadyInstalled` | `app.install` for a title that is whole on disk. Use `update` or `verify`. |
| 1031 | `SteamClientOwnsTitle` | A Windows Steam client is running against the library, or owns this title's update. |
| 1032 | `VerifyFailed` | `app.install` with `verify` found files that differ from the manifest. |
| 1033 | `BootstrapperUnverified` | `runtime.steam.install`: `SteamSetup.exe` failed its Authenticode check or its signer is not Valve Corp. The file is kept and the message names the signer found. |
| 1034 | `ClientRuntimeBroken` | The Steam client runtime is updating, broken, or never reached the login window, or `runtime.steam.repair` found nothing broken or no usable snapshot. `runtime.steam.status` says which; `runtime.steam.repair` clears a dead install's marker and, unless the client is intact after that, restores the newest usable snapshot of a broken one (a prefix record that opens and a `steam.exe`); an updating one is waited for. |
| 1035 | `ClientRedownloadRefused` | Assumption A2 failed: the client began re-downloading the title. The client was stopped and the `.acf` restored from its backup; the finding is in `meta`. |
| 1036 | `ClientLaunchTimedOut` | The Steam client reported no running title within the wait after `-applaunch`; the client was stopped. |
| 1037 | `TitleUpdatePending` | The Steam client began a routine update of the title (a build newer than the one installed), which is not a refuted A2. The client was stopped and the `.acf` restored; `app.install` with `update` brings the title current. |
<!-- error-table:end -->

A `job.failed` carries the reason code of its failure when it has one
(`ChecksumMismatch`, `InvalidBundle`, `DiskFull`, ...) and `JobFailed`
otherwise; a request refused before its job starts answers with the same
codes as an error response.

## Methods

Parameter types are in `crates/applejack-core/src/rpc/methods.rs`. Each
method has an example request in `crates/applejack-core/tests/rpc_golden/`.
`host.doctor` returns `HostFacts`, the same shape as `applejack doctor
--json`. Its `controllers` member is the pad inventory
(`[{name, transport, vendor_id, product_id, connected, kind}]`, `transport`
`usb` or `bluetooth`, `kind` one of `dualshock4`, `dualsense`, `xbox`,
`other`; an id the report lacks is `""`). When a `system_profiler` report
failed to run, timed out, or did not parse, `controllers_unavailable` carries
the reason and `controllers` is not to be read as "no pads"; the key is
absent otherwise. See `docs/controllers.md`.

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
| `steam.login.start` | implemented |
| `steam.login.complete` | implemented |
| `steam.logout` | implemented |
| `steam.library` | implemented |
| `app.install` | implemented |
| `app.run` | implemented |
| `app.cancel` | implemented |
| `runtime.steam.install` | implemented |
| `runtime.steam.update` | implemented |
| `runtime.steam.repair` | implemented |
| `runtime.steam.stop` | implemented |
| `runtime.steam.status` | implemented |
| `config.get` | implemented |
| `config.resolve` | implemented |
| `config.set` | implemented |
| `config.export` | implemented |
| `config.import` | implemented |
| `prefix.snapshot` | implemented |
| `prefix.restore` | implemented |
| `prefix.reset` | implemented |
| `session.list` | implemented |
<!-- method-table:end -->

A pending method still validates its params. Bad params get `InvalidParams`;
good params get `NotImplemented`.

### Results

The Rust types in `crates/applejack-core/src/{engines,prefixes,game_config}.rs`
and `rpc/methods.rs` define the results, and `tests/rpc_golden/<method>.result.json` carries one
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
| `steam.login.start` | `{session_id, step}`; see [Steam](#steam) |
| `steam.login.complete` | `{session_id, step}`; see [Steam](#steam) |
| `steam.logout` | `{}`. Deletes the token, the cached library, and the account name, and drops a pending login. Titles on disk stay |
| `steam.library` | `{apps: [LibraryEntry], fetched_at, job_id}`; see [Steam](#steam) |
| `app.install` | `JobStarted`; see [Steam](#steam) |
| `app.run` | `{id, log_path, job_id}`; see [Sessions](#sessions) |
| `app.cancel` | `JobStarted`; see [Sessions](#sessions) |
| `runtime.steam.install` | `JobStarted`; see [Steam client runtime](#steam-client-runtime) |
| `runtime.steam.update` | `JobStarted` |
| `runtime.steam.repair` | `JobStarted`. `reinstall` removes the client's package directory and reruns the bootstrapper |
| `runtime.steam.stop` | `JobStarted` |
| `runtime.steam.status` | `{state, prefix, build_id, running, snapshots, detail}`; `state` is one of `absent`, `installing`, `needs_login`, `ready`, `updating`, `broken` |
| `session.list` | `{"sessions": [SessionRow]}`, newest first; `appid` narrows it to one title |
| `prefix.snapshot` | `{prefix, timestamp, dir}` |
| `prefix.restore` | The same shape, naming the safety snapshot of the replaced state, or the restored snapshot when the prefix was gone and there was nothing to replace |
| `prefix.reset` | `JobStarted`. A `job.progress` message names the safety snapshot |
| `config.get` | `GameConfig`; every member `null` for an appid with no row |
| `config.resolve` | `{appid, hidraw_enabled, hidraw_source, hidraw_reason}`: the hidraw mode a run would apply now. `hidraw_source` is `config` when the row sets `hidraw_enabled`, else `controllers`, and `hidraw_reason` is the text of the run's `prepare: hidraw` line (`config`, `DualSense connected`, `DualShock 4 connected`, `no Sony pad connected`, or `controller inventory unavailable (<why>)`, in which case `hidraw_enabled` is the mode of the prefix the run would use, or `false` when that prefix is not built yet); see `docs/controllers.md` |
| `config.set` | `{}`. `value` is a string, a boolean, a number, an object for `extra_env`, or `null` to clear. `direct_verified_at` is refused: only a direct probe records it. Key `drm` sets the title's `drm` in the `apps` row, and is how a title becomes client-free; `TitleNotInstalled` when it has no row |
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
snapshot, restore, reset, remove, create, and exec operations and a running
title, so a CLI command
beside `serve` counts. The loser does not wait: it answers `PrefixBusy`.
`prefix.reset` takes the lock before it answers, so a busy or unknown prefix is
an error response, not a job, and the job holds the lock until it ends.

`prefix.restore` also works when the prefix directory is gone, which is the way
back from a failed reset, or from a removal to a snapshot: `prefix.remove`
takes none of its own. The method needs only the snapshot; a name with neither
a prefix nor a snapshot is an unknown prefix.

## Sessions

`app.run` claims a session and answers at once with the `sessions` row id, the
log path, and a job. The job's `job.progress` phases are the session's steps:
`prepare` with the hidraw decision (`hidraw disabled (DualSense connected)`,
see `docs/controllers.md`; not while a running client owns the prefix),
`prefix` (only when it builds or changes the prefix), `starting`, `running`,
`lingering` (only when the session goes on after the launch exe), then the state
it ended in. The job ends with the session, so a `job.done` means the session is
closed: `session.list` has its `exit_code`. The job of `app.cancel` reports
`cancel`, then `cancelled`; the job of a taken-over session reports `running`,
`lingering` if it applies, and the state it ended in.

A title's registered `library_dir` is an allowed root at run, so a Steam library
on another volume works: the exe stays inside the title's install directory,
that stays strictly inside the library, and `S:` of the title's prefix is the
library the prefix was created with. Everything else Wine is handed stays inside
the data directory. `prefix exec`, a CLI command and no RPC method, refuses only
an absolute Unix path outside the data directory.

A title takes the direct path with `drm = none`, with `launch_mode = direct`
and a `direct_verified_at`, or with `probe_direct`. `launch_mode = client`
wins over `drm = none`. Every other title runs through the Windows Steam
client in the `steam-client` prefix; see [Steam client runtime](#steam-client-runtime).

Refused with an error response, before anything starts or any row is written:

| Code | When |
|---|---|
| `TitleNotInstalled` | No `apps` row, no complete `appmanifest_<appid>.acf` for this appid, an empty install directory, an `installdir` that is not one plain directory name, or a relative library |
| `DownloadInProgress` | `<library>/applejack/<appid>.download.json` exists |
| `ClientRuntimeUnavailable` | The title takes the client path and the runtime is `absent` or `needs_login`; the message names `applejack runtime steam install` and the sign-in |
| `ClientRuntimeBroken` | The title takes the client path and the runtime is `installing`, `updating`, or `broken` (which includes a web helper shim with no real helper beside it, and a `cef.win*` directory with no web helper at all) |
| `NoLaunchExe` | `launch_exe_override` is empty, is not a PE image, or leaves the install directory |
| `EngineNotInstalled` | No active engine, and none pinned by the game configuration |
| `AlreadyRunning` | The title has an open session, in this core or another process |
| `PrefixBusy` | Another operation holds the prefix |
| `PrefixUnusable` | The prefix cannot be opened, or holds DLLs an older engine left |

After the answer, a failure is a `job.failed`: a session that could not start
is `failed`, with the reason in `note` and in its log, and a `wineserver` that
will not leave the prefix is `PrefixUnusable`; a run that starts the client and
finds no web helper shim in the engine payload, or a `cef.win*` directory that
lost its web helper since the answer, fails the same way, after `prepare`, with
`ClientRuntimeBroken` as the reason. A failure a cancel asked for is
`cancelled`, and its job is a `job.done`. A title that ends within five seconds
on its own, leaving nothing behind, is `exited`, and its job fails with
`TitleExitedEarly`.

Once the title is spawned, nothing closes a row over a live session. A running
stamp the database refuses is tried again at the next look, and the watch goes
on. A failure that ends the watch stops the title. If a process still remains,
the row stays open as a dead owner's, so `app.cancel` and the next core reach it.

A session is in one state, `SessionRow.state`:

| State | Means | Written by |
|---|---|---|
| `starting` | Claimed; the title has not been seen. `ended_at` is null | `app.run` |
| `running` | The game-running marker appeared; `started_at` is restamped. `ended_at` is null | The owner |
| `exited` | The marker cleared and the session drained. `exit_code` is the launch exe's, or null for a taken-over session | The owner |
| `cancelled` | It ended after `app.cancel` | The owner, or the cancel when there is none |
| `failed` | It never ran, or its watch failed and nothing of it is left | The owner |
| `lost` | The owner died and the session was gone when that was found. `exit_code` is null | The next core |

The marker ends the title, and the session goes on until it drains: no process
carries the session id and the prefix's server has gone. Until then the row is
open and the lock is held, so `app.run` answers `AlreadyRunning`, however long
that takes. A session that outlasts the ten-second bound reports `lingering`,
and closes with a note. For such a session `ended_at` is when it drained; for
the rest it is when the marker cleared.

`exit_code` is always the launch exe's status: for a launcher that exits and
leaves its game running, the launcher's, not the game's. `started_at` is when
the marker first appeared.

The process that claims a session owns it: it holds the prefix lock from before
the row exists until after it closes, on the thread that watches the title. So
`prefix.reset`, `prefix.restore`, `prefix.snapshot`, and `applejack prefix
remove` answer `PrefixBusy` for the whole session, across processes, also after
an owner died and left the session running, and only the owner can read the exit
code. `app.run` and `app.cancel` work on the session wherever it was started. A
`serve` and an `applejack app run` see each other through that lock and the row:
`serve` refuses a second launch of a title the CLI runs, lists it, and cancels
it; the CLI, which stays attached until the session ends, closes the row. It
exits with `exit_code`, with 1 when none is known, and with 1 for an early exit,
which still prints the row on stdout. `SIGINT` and `SIGTERM` cancel an attached
run; a second one stops the CLI waiting and leaves the row open.

`app.cancel` is a job, since it waits. It answers `NotRunning` for a title with
no open session. It sends `SIGTERM` to the title, `SIGKILL` three seconds later,
then waits for the rest of the session and stops the prefix's server if that
does not leave within ten seconds. `kill_prefix` skips to `<server> -k`. Three
seconds after a stop, any process still found under the session id gets `SIGKILL`,
so a session whose helpers never exit has this way out. That repeats for three
more seconds, so a process forked meanwhile goes too, and again while the job
waits for the owner. The server of a session with no process is left alone: the
owner may be building the prefix, and sees the request before the spawn, or the
server may be `prefix exec`'s. `kill_prefix` is the exception: it asks for the
whole prefix and stops that server too, which is the way out of a cancel a server
under another key holds up. When the job succeeds, no process found under the
session's `APPLEJACK_SESSION` is left and the lock is free. When one survives, the job
fails and a row with no owner stays open for the next cancel.

The kernel drops the lock when an owner dies, so an open row with a free lock
is a dead owner's. `hello` and `session.list` settle those rows. A session with
no process left closes as `lost`. One still running is taken over: the core
holds the lock on a job of its own, with no request behind it, and closes the row
when the session drains, with a null `exit_code`. The shell may ignore that job's
notifications. The CLI does not take over, since it would have to stay: its
`session list` shows the title running and `app run` refuses it, until a `serve`
starts or `app cancel` ends it. `serve` waits for its jobs at EOF, so a shell
that closes stdin leaves a core behind until the title exits; one that kills the
core leaves this recovery to the next start.

## Steam client runtime

`docs/steam-runtime.md` is the full account. `runtime.steam.status` answers
at once; the other four are jobs whose `job.progress` phases are the steps
(`prefix`, `install`, `download`, `verify`, `bootstrap`, `library`, `shim`,
`start`, `client`, `build`, `snapshot`, `repair`, `stop`), ending with a `status` phase that carries the
state. The prefix is claimed before the answer, so `PrefixBusy` (a title runs
through the client) is an error response. `runtime.steam.install` is the one
method that reaches the network outside Steam's own protocol: it fetches
Valve's `SteamSetup.exe`, refuses it with `BootstrapperUnverified` unless its
Authenticode chain verifies with Valve Corp as the signer, runs it, and
starts the client to its login window, which the user signs in to; the core
never sees credentials.

A client session is a `sessions` row with `prefix = "steam-client"`. Its job
reports `prepare` (the hidraw decision when the client is down, then the
`.acf` backup and `libraryfolders.vdf`), `shim` when
the run starts the client (the web helper shim check), `starting`,
`running` when the client's `RunningAppID` equals the appid, and the end
state. `exit_code` is null: the title's status is the client's. The client
stays up after the title, so the session does not drain and the lock is
free once the row closes. `app.cancel` ends the title and leaves the client;
`kill_prefix` stops the client too. A client that reports no running title
within ten minutes ends the job with `ClientLaunchTimedOut`; a client that
starts downloading the title ends it with `ClientRedownloadRefused` after the
`.acf` is restored from its backup (assumption A2 refuted), and one that
starts a routine patch of it (a newer build) with `TitleUpdatePending`.

## Steam

A login is a series of short requests. `steam.login.start` takes `{"mode":
{"kind": "qr"}}` or `{"mode": {"kind": "credentials", "account_name", "password"}}`,
connects, and answers `{session_id, step}`. A blank `account_name` is
`InvalidParams`, with no connection made. `steam.login.complete` takes
`session_id` and an optional `steam_guard_code`, and answers the same shape after
one submit, or one poll when there is no code. `step.kind` is below, with what a
client does. The app offers QR only, so it sees `guard_required` only when Steam
asks for a second factor on that path; it polls from the moment the first step
is answered, every two seconds, until `done`, an error, or the view goes away,
and a typed code goes out at once. The CLI drives every row.

| `kind` | Members | A client |
|---|---|---|
| `qr_ready` | `payload` | Renders it, and renders it again when it changes |
| `guard_required` | `factor`: `email_code`, `device_code`, `device_confirmation` | Shows a code field unless the factor is `device_confirmation`, and goes on polling |
| `waiting` | | Goes on polling; a login with no second factor starts here |
| `done` | `account_name` | Stops; the refresh token is stored |

Call `steam.login.complete` with no code every couple of seconds while a step is
pending, so an approval in the Steam mobile app ends the login as a typed code
does. A poll sooner than Steam's interval answers the current step without
reaching Steam. A code that arrives once none is wanted is dropped. A wrong code
answers `SteamGuardRejected` and the login stays open; any other error ends it,
and a later `complete` answers `SteamLoginFailed`. So does two minutes at
`waiting`, which ends the login too. A `session_id` that is not the pending one
answers `SteamLoginFailed` and leaves that login alone. One login is pending at a
time: `steam.login.start` drops the last. These requests block the loop for one Steam
call, bounded at 30 s, or longer while `start` tries up to three CMs. Their params
and results never reach a log.

`steam.library` answers from the cache and touches nothing else. With
`refresh: true` it answers the cache as it stood plus `job_id`, and fetches on that
job; read again after `job.done`. A `steam.logout` during the fetch stands: the
rows are dropped, and the job ends in `job.failed` with `SteamLoginRequired`.
`LibraryEntry` is `{appid, name, installdir,
size_bytes, drm, default_branch, install_state}`, and `install_state` is `installed`,
`not_installed`, or `update_available`. `update_available` compares against the
default branch, so a title on another branch stays `installed`.

`app.install` takes `{appid, branch?, update, verify, library?}`. With no `branch`,
an update stays on the branch the title was installed from. Every refusal is
an error response and starts no job: `LockHeld`, `AlreadyRunning`,
`AlreadyInstalled`, `DownloadInProgress`, `SteamClientOwnsTitle`, `InvalidParams`.
The job's `job.progress` phases are `login`, `resolve`, `download` with byte counts,
`verify`, and `register`, whose message names the launch exe, with a second
`warning:` message when that exe could not run. `app.cancel` stops an install, in
this core or another process: the install's job ends in `job.failed` with
`InstallCancelled`, the download record stays, and the next `app.install` resumes,
with `update` when an update stopped. A `verify` cannot be stopped; `app.cancel`
answers `cancelling` for one all the same.

A Steam failure that needs the user keeps its code through `job.failed`:
`SteamLoginRequired`, `AppNotOwned`, `TitleNotInstallable`, `LaunchExeAmbiguous`,
`NoLaunchExe`, `VerifyFailed`. `LockHeld` also answers a second logged-in session,
since two would displace each other; the message names the holder.

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
replaces the partial state and clears any packed `<version>.local.tar.zst` that
is not its own `from`. Only one install of an engine runs at a time, across processes
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
dropped. `app.cancel` is the method that stops work in the
core.

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
