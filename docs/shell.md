# The SwiftUI shell

`apps/Applejack/` is a Swift package with three targets: `ApplejackShell`
(the library: `CoreClient`, the RPC mirrors, the view models and views),
`ApplejackApp` (the executable, `Sources/ApplejackApp/main.swift`), and
`ApplejackShellTests`. The shell never links Rust and never spawns anything
but the core: every operation is a JSON-RPC call through `CoreClient`
(`docs/rpc-protocol.md`).

## Where the core binary is

`CoreLocator` (`Sources/ApplejackShell/CoreLocator.swift`) looks, in order,
at:

1. `APPLEJACK_CORE_BIN`, for development. A set variable is used as given:
   a path that does not exist or is not executable fails the spawn, and the
   banner reads `.stopped(launchFailed)` with the path and a Restart, so
   fixing the file and pressing Restart recovers without a relaunch.
2. `applejack-core` beside the shell executable, which in the bundle is
   `Applejack.app/Contents/MacOS/applejack-core`, when it is executable.

With neither, `AppModel.coreStatus` is `.missing` and the banner says so;
every flow then reads `failed(launchFailed)`.

## Running from a checkout

```sh
cargo build --package applejack
APPLEJACK_CORE_BIN=$PWD/target/debug/applejack \
  swift run --package-path apps/Applejack ApplejackApp
```

`swift run` starts the executable outside a bundle; `main.swift` sets the
activation policy so a window and a Dock tile still appear. For the real
bundle, see `cargo xtask app-bundle` below. `APPLEJACK_SECTION=<library|
engines|steam|logs|controllers|d3dmetal|host>` picks the view shown first
(`open --env APPLEJACK_SECTION=engines build/Applejack.app`).

## Core lifecycle in the app

`AppModel` owns the one `CoreClient`. It greets the core on launch
(`start()`), and watches `CoreClient.events()` for exits. The restart policy
stays in `CoreClient` (`docs/rpc-protocol.md`, "Restart policy"): restarts
are lazy and capped at three, and the model only reads the outcome:

- `.running(HelloResult)`: no banner.
- `.exited(status)`: the core died, whether a request was in flight or not.
  The banner's "Restart" calls `hello()` again, which respawns under the cap.
- `.stopped(error)`: `notRunning`, `launchFailed`, or `handshakeTimedOut`,
  and a `hello` whose answer could not be read (`malformedResponse`, shown
  as the binary's path and one line saying which member was wrong, or an
  `rpc` error other than the protocol mismatch). No core is running and no
  exit status was seen; under the lazy restart policy the next call
  respawns it, so the banner offers the same "Restart".
- `.failed(error)`: `restartLimit` or `protocolMismatch`. `CoreClient`
  refuses every further call, so the core is unusable until the app is
  reopened; nothing clears this state.

Any call that succeeds clears `.exited` and `.stopped` back to `.running`:
the respawn it rode on is this spawn's `hello`. A flow's `rpc` error never
reaches the banner; it belongs to the flow that made the call. "Restart" in
the banner greets the core again and, once it is running, reloads the
selected section, so the view's own failed state clears with the banner.

`CoreClient` announces an exit on `events()` from whichever path dropped the
process: the exit callback, or a teardown after a failed write (`EPIPE`
while a grandchild still holds the core's stdout), a handshake timeout, or
a `hello` that could not be read. Every open job stream ends with
`coreExited` on that announcement, so no handle survives its core and none
can adopt a job of the next core that reuses the id. A teardown of a core
that is still running reports status `-1`.

## Notification delivery

Long operations (`engine.install`, `app.install`) answer `{"job_id"}` and
then emit `job.progress`, and one `job.done` or `job.failed`. The design
chosen is a per-job stream handed out by `CoreClient.startJob`:

```swift
let handle = try await client.startJob("engine.install", params: EngineInstallParams(engineId: "rosetta-wine"))
for try await event in handle.events {   // JobEvent: .progress, .done, .failed
    ...
}
```

Behind it, the actor keeps a multicast of `CoreEvent` (`.job(JobEvent)` and
`.exited(status)`). `startJob` registers its subscription inside the actor
before the request is written, so a notification the core emits right after
answering, even one that beats the response on the wire, is delivered and
not lost. The handle's stream filters its own `job_id`, ends after the
terminal event, and throws `coreExited(status)` or `shutDown` if the core
goes away first, so a view model never waits on a job whose core is gone.
The alternative, one `notifications` stream the view model filters, was
rejected: `AsyncStream` is single-consumer, so every view model would need
its own subscription anyway, and each would re-implement the id filter and
the exit case.

`CoreClient.events()` is the unfiltered stream; `AppModel` uses it for the
exit banner. Notifications with a method the shell does not know are logged
to stderr and dropped. `JobEvent` is typed from `JobProgress`, `JobDone`,
and `JobFailed` in `RpcTypes.swift`; no stringly state.

## View models and the state enum

One `@Observable @MainActor` class per flow (the Observation framework is
macOS 14, the deployment target): `LibraryViewModel`,
`GameDetailViewModel`, `EnginesViewModel`, `SteamAccountViewModel`,
`LogsViewModel`, `ControllersViewModel`, `D3DMetalImportViewModel`, and
`HostViewModel` for `host.doctor`. Each exposes its results as

```swift
enum FlowState<Value> { case idle, loading, ready(Value), unavailable(Unavailable), failed(CoreClientError) }
```

`unavailable` is what `NotImplemented` (code 1002) maps to: `Unavailable`
carries the method from `error.data.method` and the core's message, and
`FlowStateView` renders it as "Not available in this build". Every other
error is `failed(CoreClientError)`; the brief's `failed(RpcError)` sits
inside it as `.rpc`, and `coreExited`, `restartLimit`, and the rest are
real failures a view has to show, so the enum carries the whole client
error. `AppModel.run` and `AppModel.runJob` do the mapping; cancellation
leaves a flow where it was. A job's progress is `FlowState<JobSnapshot>`:
`ready` while it runs (`finished` after `job.done`), `failed(.rpc)` with
the typed code from `job.failed`.

A flow whose method is still pending in the core shows the unavailable state.
The view models are tested against `Tests/Fixtures/fake-core.sh`, which answers every flow
method with results shaped like the Rust types when `FAKE_CORE_IMPLEMENTED=1`
is set and runs `engine.install`, `app.install`, and a `steam.library` refresh as
jobs. The result mirrors in `ShellTypes.swift` are pinned to the result goldens
and the params mirrors to the request goldens by `GoldenTests`.

Flows that need an answer before the core sends a request:

- D3DMetal import: `requestImport()` opens the license sheet unless it was
  accepted; `engine.d3dmetal.import` is sent only from
  `acknowledgeLicense()`, always with `license_acknowledged: true`. One
  acceptance covers one request: the flag is consumed by the import and
  dropped when another volume is chosen. The sheet names Apple's
  `License.pdf` on the chosen volume (plan 02, "D3DMetal user supply"),
  opens it, and does not paraphrase its terms; "I accept" stays disabled
  until that file exists.
- Preset import: `config.import` is sent with `accept_launch_overrides:
  false`. When the result is not applied and lists confirmations, the view
  presents an alert with the pending plan (confirmations and rejections) as
  its data, and "Apply overrides" calls `confirmLaunchOverrides(path:)` with
  the path from that data. SwiftUI writes `false` to the alert's binding,
  which declines, before the button's task runs, so the confirm must not
  read the path back from the model; `declineLaunchOverrides()` is a no-op
  once nothing is pending. Cancel keeps the preview on screen, marked as
  declined, so the rejected items stay visible. A preview answered after a
  newer preset's preview is dropped, so the alert never offers an earlier
  preset's overrides.

Run with log tail: "Run" in the game detail sends `app.run` (`AppRunResult`:
`id`, `log_path`, and the `job_id` that reports the session). The button is
disabled until the core answers, which it does before the title is on
screen, and then while the session is open. If the user is still on the
detail then, the shell switches to Logs and follows it; the library's
navigation path lives in `ShellModels`, so returning to Library lands on the
same detail. `AppModel.detail(for:)` keeps one `GameDetailViewModel` per
title for the app's lifetime, so the rebuilt detail finds its run and its
session, and two views of one title cannot each send `app.run`. The run's
job is followed on a task of the model's own (`CoreClient.startJob(_:params:jobId:)`
subscribes before the request is written, so an early phase is not lost):
`session` shows the phases and `finished` once the row closed. A run whose
start was cancelled leaves `runState` idle, never loading. The core refuses a
second launch of a running title with `AlreadyRunning`.

Cancel: "Cancel" sends `app.cancel` for a session this model started, and,
after a confirmation, for an open row `session.list` showed that some other
process owns (`openSession`). Its job is followed in `cancelState`
(`cancel`, `cancelled`), and the sessions are read again when it ends.
`NotRunning` and the other refusals show the core's message.

Export: "Export preset…" opens a save panel and sends `config.export` with
the chosen path; the core applies the export denylist and writes the file.

Prefix snapshots (`PrefixSnapshotsViewModel`, shown on the game detail for
the title's prefix and under the Steam client for `steam-client`): Snapshot
shows the timestamp `prefix.snapshot` made; Restore takes a timestamp from a
text field or from the picker of timestamps this model saw this session
(there is no listing method; the follow-up is in `docs/rpc-protocol.md`,
"Jobs"); Reset is a job whose safety snapshot is read from its progress
line, and a failed reset's message names the snapshot to restore, which the
view offers as a button.

Steam client runtime (`SteamClientRuntimeViewModel`, under the account on
the Steam view, since `needs_login` is a sign-in too): `runtime.steam.status`
is read when the view appears and after every action; Install, Update,
Repair, Reinstall (`repair` with `reinstall: true`) and Stop run one at a
time as jobs, and every `job.progress` line is listed, not only the latest,
because the `start` phase carries the macOS microphone warning the user has
about 15 s to act on. A `job.failed` (`ClientRuntimeBroken` and the other
103x codes) shows the core's message verbatim; it names the remedy.

Logs: `session.list` names each session's `log_path`; `LogsViewModel`
follows it with `LogTailer` (a polling file reader, so a log that does not
exist yet is picked up when the engine creates it, and one that shrank or
changed inode was rotated and is read again from the start). Leaving the
Logs view suspends the poll; coming back resumes it from the start of the
file, and lines the old tail had buffered are dropped rather than appended
to the new one. Controllers come from the GameController framework, not
the core.

Steam account: there is no `steam.status` method, so before any call the
view says the sign-in state is unknown rather than "not signed in". Once
`steam.login.start` answers, the model polls `steam.login.complete` every
two seconds on a task of its own, from the moment the challenge is up: an
approval in the Steam mobile app ends the login without a button. The loop
ends on `done`, on an error, when the view disappears (`stopPolling()`; it
resumes when the view comes back), and when another login starts. A
`guard_required` step shows a code field unless the factor is
`device_confirmation`; Submit restarts the loop so the code goes out at
once. The shell logs nothing of these requests; the core redacts them.

The `config.import` result the shell expects (`applied`, `accepted`,
`rejected`, `confirmations`) is the shell's proposal for how
`ImportPlan::apply`'s `ConfirmationRequired` crosses the wire; it is
recorded in `docs/rpc-protocol.md` ("Methods") for the 6a RPC unit.

## `cargo xtask app-bundle`

Builds the shell (`swift build -c release --product ApplejackApp`) and the
core (`cargo build --package applejack --release`) and assembles

```text
build/Applejack.app/
  Contents/Info.plist          apps/Applejack/Resources/Info.plist
  Contents/PkgInfo             APPL????
  Contents/MacOS/Applejack     the ApplejackApp product
  Contents/MacOS/applejack-core  the applejack binary
  Contents/Resources/          empty until the shell has resources
```

`xtask/src/app_bundle.rs` stages the tree under `build/Applejack.app.tmp`
and swaps it into place (a stale `.old` from an interrupted run is put back
first), like `build/assemble.sh` does for the engine payload. `--debug`
builds both sides in debug and writes `build/Applejack-debug.app`; `cargo
xtask check` runs that and verifies the layout (`CFBundleExecutable`,
`CFBundleIdentifier`, `LSMinimumSystemVersion` as key and value pairs).
The step reuses `.build/` and `target/`, so after the `swift-build` and
`core-build` steps it is a copy, about a second on this host, not a second
Swift build. `build/` also holds tracked files (`assemble.sh`, `pins.toml`,
the fetch scripts); never remove the directory to reset the bundle, only
`build/Applejack*.app`.
Nothing is signed with an identity: `open build/Applejack.app` works on the
building machine because the binaries carry the linker's ad-hoc signature
and no quarantine attribute. That ad-hoc signature names the binaries
`ApplejackApp-<hash>` and `applejack-<hash>`, and the debug product's carries
`com.apple.security.get-task-allow`; 6d re-signs both with the repository
entitlements file and explicit identifiers, so neither detail survives into
a release.

Work package 6d adds signing (Developer ID, hardened runtime, both core
copies with identifier `com.applejack.core`, the entitlements file
`apps/Applejack/Resources/Applejack.entitlements`, which must never gain a
code-signing exception entitlement), notarization, stapling, and the DMG.
