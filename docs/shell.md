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
  and a `hello` whose answer could not be read (`malformedResponse`, or an
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

The core implements only `hello` and `host.doctor` today, so in the running
app every flow but Host shows the unavailable state. The view models are
tested against `Tests/Fixtures/fake-core.sh`, which answers every flow
method with results shaped like the Rust types when `FAKE_CORE_IMPLEMENTED=1`
is set and runs `engine.install` and `app.install` as jobs. The result
mirrors for pending methods (`ShellTypes.swift`: `SteamLibraryResult`,
`EngineListResult`, `DoctorReport`, `LoginSession`, `SessionListResult`,
`GameConfig`, `ConfigImportResult`) are provisional until the methods land
and gain result goldens; the params mirrors are pinned to the request
goldens by `GoldenTests`.

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
`id` and `log_path`, the shell's proposal until work package 5 defines the
result, named after the engine's `Session` fields). One run is in flight
per title at a time; the button is disabled meanwhile. When the result
names a `log_path` and the user is still on the detail, the shell switches
to Logs and follows it; the library's navigation path lives in
`ShellModels`, so returning to Library lands on the same detail. `app.run`
is `NotImplemented` until work package 5, so today the action renders the
unavailable state.

Logs: `session.list` names each session's `log_path`; `LogsViewModel`
follows it with `LogTailer` (a polling file reader, so a log that does not
exist yet is picked up when the engine creates it, and one that shrank or
changed inode was rotated and is read again from the start). Leaving the
Logs view suspends the poll; coming back resumes it from the start of the
file, and lines the old tail had buffered are dropped rather than appended
to the new one. Controllers come from the GameController framework, not
the core.

Steam account: there is no `steam.status` method, so before any call the
view says the sign-in state is unknown rather than "not signed in".

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
