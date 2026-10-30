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

1. `APPLEJACK_CORE_BIN`, for development. The path must be executable.
2. `applejack-core` beside the shell executable, which in the bundle is
   `Applejack.app/Contents/MacOS/applejack-core`.

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
- `.failed(error)`: `restartLimit`, `launchFailed`, `handshakeTimedOut`, or
  `protocolMismatch`. The core is unusable until the app is reopened.

An `rpc` error never reaches the banner; it belongs to the flow that made
the call.

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
  `acknowledgeLicense()`, always with `license_acknowledged: true`.
- Preset import: `config.import` is sent with `accept_launch_overrides:
  false`. When the result is not applied and lists confirmations, the view
  shows them (field and value) and `confirmLaunchOverrides()` sends the
  request again with `true`.

Logs: `session.list` names each session's `log_path`; `LogsViewModel`
follows it with `LogTailer` (a polling file reader, so a log that does not
exist yet is picked up when the engine creates it). Controllers come from
the GameController framework, not the core.

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
xtask check` runs that and verifies the layout. Nothing is signed:
`open build/Applejack.app` works on the building machine because the binaries
carry the linker's ad-hoc signature and no quarantine attribute.

Work package 6d adds signing (Developer ID, hardened runtime, both core
copies with identifier `com.applejack.core`, the entitlements file
`apps/Applejack/Resources/Applejack.entitlements`, which must never gain a
code-signing exception entitlement), notarization, stapling, and the DMG.
