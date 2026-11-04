import CoreGraphics
import Foundation
import XCTest

@testable import ApplejackShell

/// The view models against `Tests/Fixtures/fake-core.sh`. Without
/// `FAKE_CORE_IMPLEMENTED` the fake answers like today's core: every flow
/// method is `NotImplemented`.
@MainActor
final class ViewModelTests: XCTestCase {
    /// One directory per test for the fake's request logs: a fake still
    /// draining its pipe appends after `tearDown`, and an append into a
    /// removed directory leaves nothing behind.
    nonisolated let logDir = FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-requests-\(UUID().uuidString)", isDirectory: true)
    nonisolated var requestLog: URL { logDir.appendingPathComponent("requests.log") }
    /// For a test's second app. `shutdown` only closes the first fake's
    /// stdin; a request already in the pipe is read and logged after
    /// `shutdown` returns, so a count over one shared log would take the
    /// first fake's last request for the second app's.
    nonisolated var secondRequestLog: URL { logDir.appendingPathComponent("second.log") }

    override func setUp() {
        super.setUp()
        // Made here, not in the property: XCTest instantiates the class once
        // per test method to build the suite, and only the instances it runs
        // reach `tearDown`.
        try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
    }

    override func tearDown() {
        try? FileManager.default.removeItem(at: logDir)
        super.tearDown()
    }

    /// The fake logs its requests to `requestLog` unless `environment`
    /// names another file.
    func makeApp(implemented: Bool = true, environment: [String: String] = [:]) -> AppModel {
        var env = environment
        if env["FAKE_CORE_REQUEST_LOG"] == nil {
            env["FAKE_CORE_REQUEST_LOG"] = requestLog.path
        }
        if implemented {
            env["FAKE_CORE_IMPLEMENTED"] = "1"
        }
        return AppModel(client: CoreClient(executable: CoreClientTests.fakeCore, arguments: [], environment: env))
    }

    func requests(in log: URL? = nil) throws -> [String] {
        let log = log ?? requestLog
        guard FileManager.default.fileExists(atPath: log.path) else { return [] }
        return try String(contentsOf: log, encoding: .utf8).split(separator: "\n").map(String.init)
    }

    func requests(for method: String, in log: URL? = nil) throws -> [String] {
        try requests(in: log).filter { $0.contains("\"method\":\"\(method)\"") }
    }

    /// The first request for `method`; a failure, not a crash, when none was sent.
    func request(for method: String, file: StaticString = #filePath, line: UInt = #line) throws -> String {
        try XCTUnwrap(requests(for: method).first, "no `\(method)` request was sent", file: file, line: line)
    }

    // MARK: - NotImplemented maps to unavailable

    func testNotImplementedMapsToUnavailable() async throws {
        let app = makeApp(implemented: false)
        let library = LibraryViewModel(app: app)
        await library.load()
        XCTAssertEqual(library.library, .unavailable(Unavailable(method: "steam.library", message: "`steam.library` is pending (fake core)")))

        let engines = EnginesViewModel(app: app)
        await engines.load()
        guard case .unavailable(let reason) = engines.engines else {
            return XCTFail("expected unavailable, got \(engines.engines)")
        }
        XCTAssertEqual(reason.method, "engine.list")
        await engines.install("rosetta-wine")
        guard case .unavailable(let installReason) = engines.install else {
            return XCTFail("a NotImplemented job method must read as unavailable, got \(engines.install)")
        }
        XCTAssertEqual(installReason.method, "engine.install")

        let steam = SteamAccountViewModel(app: app)
        await steam.startQrLogin()
        guard case .unavailable = steam.login else { return XCTFail("expected unavailable, got \(steam.login)") }

        let logs = LogsViewModel(app: app)
        await logs.load()
        guard case .unavailable = logs.sessions else { return XCTFail("expected unavailable, got \(logs.sessions)") }

        let host = HostViewModel(app: app)
        await host.load()
        XCTAssertEqual(host.facts.value?.arch, "arm64", "host.doctor is implemented and must stay ready")
        if case .running = app.coreStatus {} else { XCTFail("NotImplemented must not touch the core banner: \(app.coreStatus)") }
        await app.shutdown()
    }

    func testUnavailableIsOnlyNotImplemented() {
        XCTAssertNotNil(Unavailable(.rpc(RpcError(code: RpcErrorCode.notImplemented.rawValue, message: "pending", data: .object(["method": .string("x")])))))
        XCTAssertNil(Unavailable(.rpc(RpcError(code: RpcErrorCode.internal.rawValue, message: "boom"))))
        XCTAssertNil(Unavailable(.coreExited(1)))
        let state: FlowState<Int> = .failure(.rpc(RpcError(code: RpcErrorCode.invalidParams.rawValue, message: "bad")))
        guard case .failed = state else { return XCTFail("InvalidParams is a failure, got \(state)") }
    }

    // MARK: - Library and config

    func testLibraryListsInstallState() async throws {
        let app = makeApp()
        let library = LibraryViewModel(app: app)
        await library.load(refresh: true)
        let apps = try XCTUnwrap(library.library.value?.apps)
        XCTAssertEqual(apps.map(\.appid), [220, 440])
        XCTAssertEqual(apps.map(\.installState), [.installed, .notInstalled])
        XCTAssertEqual(apps[0].drm, .steam)
        XCTAssertTrue(try request(for: "steam.library").contains("\"params\":{\"refresh\":true}"))

        await library.install(440)
        XCTAssertNil(library.installs[440], "a finished install hands the row back to the library's install state")
        XCTAssertTrue(try request(for: "app.install").contains("\"params\":{\"appid\":440,\"update\":false,\"verify\":false}"))
        XCTAssertEqual(try requests(for: "steam.library").count, 3, "a refresh is a job and a read; a finished install reloads the library")
        await app.shutdown()
    }

    func testConfigEditorLoadsAndSets() async throws {
        let app = makeApp()
        let detail = GameDetailViewModel(app: app, appid: 220)
        await detail.load()
        let config = try XCTUnwrap(detail.config.value)
        XCTAssertEqual(config.graphicsBackend, .dxmt)
        XCTAssertEqual(config.syncMode, .esync)
        XCTAssertEqual(config.launchMode, .client)
        XCTAssertEqual(config.extraEnv, ["DXMT_LOG": "1"])
        XCTAssertNil(config.steamInput)
        XCTAssertEqual(config.value(.notes), .string("fake"))
        XCTAssertEqual(config.value(.steamInput), .null)

        await detail.set(.graphicsBackend, to: .string("wined3d"))
        XCTAssertEqual(detail.lastSave, .ready(.object([:])))
        XCTAssertTrue(try request(for: "config.set").contains("\"params\":{\"appid\":220,\"key\":\"graphics_backend\",\"value\":\"wined3d\"}"))
        XCTAssertEqual(try requests(for: "config.get").count, 2, "a save reloads the config")
        await app.shutdown()
    }

    func testPresetImportAsksBeforeApplyingLaunchOverrides() async throws {
        let app = makeApp()
        let detail = GameDetailViewModel(app: app, appid: 220)
        await detail.importPreset(at: "/tmp/220.preset.json")
        XCTAssertEqual(detail.pendingConfirmation, [ImportItem(field: "launch_exe_override", value: "bin/game.exe")])
        XCTAssertEqual(detail.importState.value?.applied, false)
        var imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 1)
        XCTAssertTrue(try XCTUnwrap(imports.first).contains("\"accept_launch_overrides\":false"))

        await detail.confirmLaunchOverrides()
        XCTAssertTrue(detail.pendingConfirmation.isEmpty)
        XCTAssertEqual(detail.importState.value?.applied, true)
        imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 2)
        XCTAssertTrue(try XCTUnwrap(imports.dropFirst().first).contains("\"accept_launch_overrides\":true"))
        XCTAssertEqual(try requests(for: "config.get").count, 1, "an applied import reloads the config")

        // Declining sends nothing and keeps the preview on screen.
        await detail.importPreset(at: "/tmp/220.preset.json")
        detail.declineLaunchOverrides()
        XCTAssertEqual(try requests(for: "config.import").count, 3)
        XCTAssertEqual(detail.importState.value?.applied, false)
        XCTAssertEqual(detail.importState.value?.rejected.map(\.field), ["extra_env.VK_ICD_FILENAMES"])
        XCTAssertNil(detail.pendingImport)
        await app.shutdown()
    }

    func testALatePreviewDoesNotReplaceTheNewerPendingImport() async throws {
        // The fake previews a path containing "slow" after 400 ms (lifted
        // from the round-two review, probe PK).
        let app = makeApp()
        let detail = GameDetailViewModel(app: app, appid: 220)
        let slow = Task { await detail.importPreset(at: "/tmp/slow-A.preset.json") }
        try await Task.sleep(for: .milliseconds(100))
        await detail.importPreset(at: "/tmp/B.preset.json")
        XCTAssertEqual(detail.pendingImport?.path, "/tmp/B.preset.json")
        await slow.value
        XCTAssertEqual(detail.pendingImport?.path, "/tmp/B.preset.json", "the earlier preset's late preview must not replace B's plan")
        XCTAssertEqual(detail.pendingConfirmation.map(\.value), ["bin/game.exe"])
        XCTAssertEqual(detail.importState.value?.confirmations.map(\.value), ["bin/game.exe"])

        await detail.confirmLaunchOverrides()
        let imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 3)
        XCTAssertTrue(try XCTUnwrap(imports.dropFirst(2).first).contains("\"accept_launch_overrides\":true,\"appid\":220,\"path\":\"/tmp/B.preset.json\""), "\(imports)")
        await app.shutdown()
    }

    func testAPresetForAnotherTitleIsCalledOut() async throws {
        // The fake answers `preset_appid` 730 for a path containing "foreign".
        let app = makeApp()
        let detail = GameDetailViewModel(app: app, appid: 220)
        await detail.importPreset(at: "/tmp/foreign.preset.json")
        let pending = try XCTUnwrap(detail.pendingImport)
        XCTAssertEqual(pending.presetAppid, 730)
        XCTAssertEqual(
            detail.presetMismatch(presetAppid: pending.presetAppid), "This preset was exported for app 730, not this game.")

        await detail.confirmLaunchOverrides()
        let applied = try XCTUnwrap(detail.importState.value)
        XCTAssertTrue(applied.applied)
        XCTAssertNotNil(detail.presetMismatch(presetAppid: applied.presetAppid), "the applied status keeps the warning")

        await detail.importPreset(at: "/tmp/220.preset.json")
        let own = try XCTUnwrap(detail.pendingImport)
        XCTAssertEqual(own.presetAppid, 220)
        XCTAssertNil(detail.presetMismatch(presetAppid: own.presetAppid))
        await app.shutdown()
    }

    func testConfirmingThroughTheAlertSurvivesItsDismissal() async throws {
        // The alert's confirm button holds the value it was presented with.
        // SwiftUI then writes `false` to `isPresented` (the view declines)
        // before the button's task runs, in that order; the confirmed import
        // must still go out with `accept_launch_overrides: true`.
        let app = makeApp()
        let detail = GameDetailViewModel(app: app, appid: 220)
        await detail.importPreset(at: "/tmp/220.preset.json")
        let presented = try XCTUnwrap(detail.pendingImport)
        XCTAssertEqual(presented.path, "/tmp/220.preset.json")
        XCTAssertEqual(presented.confirmations, [ImportItem(field: "launch_exe_override", value: "bin/game.exe")])
        XCTAssertEqual(presented.rejected.map(\.field), ["extra_env.VK_ICD_FILENAMES"])

        detail.declineLaunchOverrides()
        XCTAssertNil(detail.pendingImport)
        XCTAssertEqual(detail.importState.value?.applied, false, "a decline keeps the preview")
        await detail.confirmLaunchOverrides(path: presented.path)
        XCTAssertEqual(detail.importState.value?.applied, true)
        let imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 2)
        XCTAssertTrue(try XCTUnwrap(imports.dropFirst().first).contains("\"accept_launch_overrides\":true"), "\(imports)")
        XCTAssertEqual(try requests(for: "config.get").count, 1, "the applied import reloads the config")

        // The other order: a dismissal after the confirm took the plan is a no-op.
        await detail.importPreset(at: "/tmp/220.preset.json")
        let again = try XCTUnwrap(detail.pendingImport)
        await detail.confirmLaunchOverrides(path: again.path)
        detail.declineLaunchOverrides()
        XCTAssertEqual(detail.importState.value?.applied, true, "a late dismissal must not reset the applied import")
        XCTAssertEqual(try requests(for: "config.import").count, 4)
        await app.shutdown()
    }

    func testRunSendsAppRunAndNamesTheSessionLog() async throws {
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("fake-run-\(UUID().uuidString).log")
        let app = makeApp(environment: ["FAKE_CORE_SESSION_LOG": log.path])
        let detail = GameDetailViewModel(app: app, appid: 220)
        await detail.run()
        XCTAssertEqual(detail.runState.value, AppRunResult(id: 41, logPath: log.path, jobId: 12))
        XCTAssertTrue(try request(for: "app.run").contains("\"params\":{\"appid\":220,\"probe_direct\":false}"))
        await app.shutdown()

        // Today's core: the flow reads as unavailable, not as a failure.
        let pending = makeApp(implemented: false)
        let detailPending = GameDetailViewModel(app: pending, appid: 220)
        await detailPending.run()
        guard case .unavailable(let reason) = detailPending.runState else {
            return XCTFail("expected unavailable, got \(detailPending.runState)")
        }
        XCTAssertEqual(reason.method, "app.run")
        await pending.shutdown()
    }

    func testASecondRunWhileOneIsInFlightSendsNothing() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUN_DELAY_MS": "300", "FAKE_CORE_RUN_OUTCOME": "exits"])
        let detail = app.detail(for: 220)
        XCTAssertTrue(detail === app.detail(for: 220), "one detail model per title")
        let first = Task { await detail.run() }
        try await waitUntil { detail.runState == .loading }
        await app.detail(for: 220).run()
        XCTAssertEqual(detail.runState, .loading, "the second Run returns at once and leaves the first in flight")
        await first.value
        XCTAssertNotNil(detail.runState.value)
        XCTAssertEqual(try requests(for: "app.run").count, 1)

        // While the session is open, Run sends nothing; once it closed, Run works again.
        try await waitUntil { detail.session.value != nil }
        if detail.isRunning {
            await detail.run()
            XCTAssertEqual(try requests(for: "app.run").count, 1)
        }
        try await waitUntil { detail.session.value?.finished == true }
        XCTAssertFalse(detail.isRunning)
        await detail.run()
        XCTAssertEqual(try requests(for: "app.run").count, 2)
        await app.shutdown()
    }

    /// No view cancels the task around `run()`; if one did, the request
    /// lives on the follow task, so the caller returns at the core's answer
    /// and the run goes on to its end.
    func testACancelledRunLeavesRunInATerminalState() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUN_DELAY_MS": "200", "FAKE_CORE_RUN_OUTCOME": "exits"])
        let detail = app.detail(for: 220)
        let run = Task { await detail.run() }
        try await waitUntil { detail.runState == .loading }
        run.cancel()
        await run.value
        XCTAssertEqual(detail.runState.value?.id, 41, "the caller returns at the answer, not at the cancellation: \(detail.runState)")
        try await waitUntil { detail.session.value?.finished == true }
        XCTAssertFalse(detail.isRunning)
        XCTAssertNil(detail.openSession, "the exited row is not an open one")
        await detail.run()
        XCTAssertEqual(try requests(for: "app.run").count, 2, "Run works again after the first run ended")
        await app.shutdown()
    }

    // MARK: - Cancel

    func testCancelEndsTheSessionThisShellStarted() async throws {
        let app = makeApp()
        let detail = app.detail(for: 220)
        await detail.run()
        XCTAssertEqual(detail.runState.value?.id, 41)
        try await waitUntil { detail.session.value?.phase == "running" }
        XCTAssertTrue(detail.isRunning)
        XCTAssertTrue(detail.canCancel)
        XCTAssertNil(detail.openSession, "the shell's own run is not another process's open row")

        await detail.cancel()
        XCTAssertEqual(detail.cancelState.value?.finished, true)
        XCTAssertEqual(detail.cancelState.value?.phase, "cancelled")
        XCTAssertTrue(try request(for: "app.cancel").contains("\"params\":{\"appid\":220,\"kill_prefix\":false}"))
        try await waitUntil { detail.session.value?.finished == true }
        XCTAssertFalse(detail.isRunning)
        XCTAssertEqual(detail.session.value?.phase, "cancelled")
        XCTAssertNil(detail.openSession, "the row the cancel closed is not offered again")
        XCTAssertFalse(detail.canCancel)
        await app.shutdown()
    }

    func testASecondStopWhileTheCancelJobRunsSendsNothing() async throws {
        let app = makeApp()
        let detail = app.detail(for: 220)
        await detail.run()
        try await waitUntil { detail.session.value?.phase == "running" }
        let first = Task { await detail.cancel() }
        try await waitUntil { detail.cancelState.value.map { !$0.finished } ?? false }
        XCTAssertFalse(detail.canCancel, "Stop is not offered while its own job runs: \(detail.cancelState)")
        await detail.cancel()
        XCTAssertEqual(try requests(for: "app.cancel").count, 1, "the second press sends nothing")
        await first.value
        XCTAssertEqual(detail.cancelState.value?.finished, true)
        await app.shutdown()
    }

    func testAListReadDuringASlowStartDoesNotOfferTheOwnRow() async throws {
        // The core inserts the row before it answers `app.run`; the fake
        // does the same.
        let app = makeApp(environment: ["FAKE_CORE_RUN_DELAY_MS": "300"])
        let detail = app.detail(for: 220)
        let run = Task { await detail.run() }
        try await waitUntil { detail.runState == .loading }
        await detail.loadSessions()
        XCTAssertNil(detail.openSession, "the row of the start in flight is the shell's own")
        await run.value
        XCTAssertEqual(detail.runState.value?.id, 41)
        try await waitUntil { detail.isRunning }
        await detail.loadSessions()
        XCTAssertNil(detail.openSession)
        XCTAssertTrue(detail.canCancel)
        await detail.cancel()
        try await waitUntil { detail.session.value?.finished == true }
        XCTAssertNil(detail.openSession)
        await app.shutdown()
    }

    func testCancelOffersAnOpenRowFromSessionListAndShowsARefusal() async throws {
        // The fake lists an open session (id 1, running) for 220.
        let app = makeApp()
        let detail = app.detail(for: 220)
        await detail.load()
        XCTAssertEqual(detail.openSession?.id, 1)
        XCTAssertFalse(detail.isRunning)
        XCTAssertTrue(detail.canCancel)
        await detail.cancel(killPrefix: true)
        XCTAssertEqual(detail.cancelState.value?.finished, true)
        XCTAssertTrue(try request(for: "app.cancel").contains("\"kill_prefix\":true"))
        XCTAssertEqual(try requests(for: "session.list").count, 2, "a finished cancel reads the sessions again")
        XCTAssertNil(detail.openSession, "the cancel closed the row")
        XCTAssertFalse(detail.canCancel)
        await app.shutdown()

        let refusing = makeApp(environment: ["FAKE_CORE_CANCEL_REFUSED": "1"])
        let other = refusing.detail(for: 220)
        await other.cancel()
        XCTAssertEqual(other.cancelState, .failed(.rpc(RpcError(code: RpcErrorCode.notRunning.rawValue, message: "app 220 has no open session"))))
        await refusing.shutdown()
    }

    /// `SessionRow` (`sessions.rs`): `starting` at the claim, `running` once
    /// the marker appears, `cancel_requested` from the cancel, and the end
    /// state with `ended_at`; the fake's row moves the same way, so a shell
    /// that reads `state` or `cancel_requested` learns what the core says.
    func testTheSessionRowFollowsRunCancelAndExit() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUN_DELAY_MS": "300"])
        let logs = LogsViewModel(app: app)
        func row() async throws -> SessionEntry {
            await logs.load(appid: 220)
            return try XCTUnwrap(logs.sessions.value?.sessions.first)
        }
        var seen = try await row()
        XCTAssertEqual(seen.id, 1)
        XCTAssertEqual(seen.state, .running)
        XCTAssertFalse(seen.cancelRequested)

        let detail = app.detail(for: 220)
        let run = Task { await detail.run() }
        try await waitUntil { detail.runState == .loading }
        seen = try await row()
        XCTAssertEqual(seen.id, 41)
        XCTAssertEqual(seen.state, .starting, "claimed, not yet seen running")
        XCTAssertNil(seen.endedAt)
        XCTAssertFalse(seen.cancelRequested)
        await run.value
        try await waitUntil { detail.session.value?.phase == "running" }
        seen = try await row()
        XCTAssertEqual(seen.state, .running)
        XCTAssertNil(seen.endedAt)

        await detail.cancel()
        try await waitUntil { detail.session.value?.finished == true }
        seen = try await row()
        XCTAssertEqual(seen.id, 41)
        XCTAssertEqual(seen.state, .cancelled)
        XCTAssertTrue(seen.cancelRequested, "the cancel flagged the row before its job closed it")
        XCTAssertNotNil(seen.endedAt)
        XCTAssertEqual(seen.exitCode, 143, "the owner was alive, so it closed the row with the status it reaped from the title the cancel ended, SIGTERM (`PlannedRun::close`); only a dead owner's row has none")
        await app.shutdown()

        let exiting = makeApp(environment: ["FAKE_CORE_RUN_OUTCOME": "exits"])
        let other = exiting.detail(for: 220)
        await other.run()
        try await waitUntil { other.session.value?.finished == true }
        let more = LogsViewModel(app: exiting)
        await more.load(appid: 220)
        let ended = try XCTUnwrap(more.sessions.value?.sessions.first)
        XCTAssertEqual(ended.id, 41)
        XCTAssertEqual(ended.state, .exited)
        XCTAssertEqual(ended.exitCode, 0)
        XCTAssertFalse(ended.cancelRequested)
        XCTAssertNotNil(ended.endedAt)
        await exiting.shutdown()
    }

    /// The core's owner closes a row by its flag (`PlannedRun::close`): a
    /// cancel asked for before the title exited ends the row `cancelled`,
    /// and the owner still records the exit it reaped. The fake's cancel job
    /// is held past the exit, so the run job is the one that closes the row.
    func testACancelBeforeTheExitEndsTheRowCancelled() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUN_OUTCOME": "exits", "FAKE_CORE_CANCEL_CLOSE_MS": "500"])
        let detail = app.detail(for: 220)
        await detail.run()
        await detail.cancel()
        try await waitUntil { detail.session.value?.finished == true }
        XCTAssertEqual(detail.session.value?.phase, "cancelled", "job 12 reports the end state the flag picked")
        XCTAssertEqual(detail.cancelState.value?.finished, true)
        let logs = LogsViewModel(app: app)
        await logs.load(appid: 220)
        let row = try XCTUnwrap(logs.sessions.value?.sessions.first)
        XCTAssertEqual(row.id, 41)
        XCTAssertEqual(row.state, .cancelled)
        XCTAssertTrue(row.cancelRequested)
        XCTAssertEqual(row.exitCode, 0, "the owner reaped the title, so the row carries its status whatever the state (`PlannedRun::close`)")
        XCTAssertNotNil(row.endedAt)
        await app.shutdown()
    }

    func testExportSendsThePathAndReportsWhereTheCoreWrote() async throws {
        let app = makeApp()
        let detail = app.detail(for: 220)
        await detail.export(to: "/tmp/out/220.preset.json")
        XCTAssertEqual(detail.exportState.value, ConfigExportResult(path: "/tmp/out/220.preset.json"))
        XCTAssertTrue(try XCTUnwrap(requests(for: "config.export").first).contains("\"params\":{\"appid\":220,\"path\":\"/tmp/out/220.preset.json\"}"))
        await app.shutdown()
    }

    // MARK: - Steam client runtime

    func testRuntimeStatusAndEachActionFollowsItsJob() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUNTIME_STATE": "absent"])
        let runtime = SteamClientRuntimeViewModel(app: app)
        await runtime.load()
        let status = try XCTUnwrap(runtime.status.value)
        XCTAssertEqual(status.state, .absent)
        XCTAssertNil(status.buildId)
        XCTAssertEqual(status.snapshots, 0)
        XCTAssertEqual(status.detail, "no steam-client prefix")

        let expected: [(SteamClientRuntimeViewModel.Action, String, String)] = [
            (.install, "runtime.steam.install", "\"params\":{}"),
            (.update, "runtime.steam.update", "\"params\":{}"),
            (.repair, "runtime.steam.repair", "\"params\":{\"reinstall\":false}"),
            (.reinstall, "runtime.steam.repair", "\"params\":{\"reinstall\":true}"),
            (.stop, "runtime.steam.stop", "\"params\":{}"),
        ]
        for (index, (action, method, params)) in expected.enumerated() {
            await runtime.perform(action)
            XCTAssertEqual(runtime.action, action)
            XCTAssertEqual(runtime.job.value?.finished, true, "\(action)")
            XCTAssertEqual(runtime.progress.map(\.phase), ["prefix", "start", "status"], "\(action)")
            let start = try XCTUnwrap(runtime.progress.first { $0.phase == "start" }, "\(action)")
            XCTAssertTrue(start.message.contains("microphone"), start.message)
            let sent = try requests(for: method)
            XCTAssertTrue(try XCTUnwrap(sent.last).contains(params), "\(action): \(sent)")
            XCTAssertEqual(try requests(for: "runtime.steam.status").count, index + 2, "\(action) reads the status again when it ends")
        }
        await app.shutdown()
    }

    func testARuntimeJobThatFailsKeepsItsStartLineAndTheCoreMessage() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUNTIME_OUTCOME": "broken"])
        let runtime = SteamClientRuntimeViewModel(app: app)
        let busy = Task { await runtime.perform(.install) }
        try await waitUntil { runtime.job.value != nil }
        await runtime.perform(.update)
        XCTAssertTrue(try requests(for: "runtime.steam.update").isEmpty, "one runtime job at a time")
        await busy.value
        XCTAssertEqual(
            runtime.job,
            .failed(.rpc(RpcError(code: RpcErrorCode.clientRuntimeBroken.rawValue, message: "the Steam client never reached the login window; run `applejack runtime steam repair`"))))
        XCTAssertEqual(runtime.progress.map(\.phase), ["prefix", "start"])
        XCTAssertTrue(try XCTUnwrap(runtime.progress.last).message.contains("Don't Allow is fine"))
        XCTAssertEqual(try requests(for: "runtime.steam.status").count, 1)
        await app.shutdown()
    }

    func testRuntimeButtonsFollowTheStatus() async throws {
        typealias Action = SteamClientRuntimeViewModel.Action
        let all: [Action] = [.install, .update, .repair, .reinstall, .stop]
        // Stop follows `running`, not the state: a client left running by a
        // signed-in account is `ready` with `running: true`.
        let expected: [(String, String?, [Action])] = [
            ("absent", nil, [.install]),
            ("ready", nil, [.install, .update, .repair, .reinstall]),
            ("ready", "1", all),
            ("needs_login", nil, all),
            ("needs_login", "0", [.install, .update, .repair, .reinstall]),
            ("broken", nil, [.install, .update, .repair, .reinstall]),
        ]
        for (state, running, enabled) in expected {
            var environment = ["FAKE_CORE_RUNTIME_STATE": state]
            if let running { environment["FAKE_CORE_RUNTIME_RUNNING"] = running }
            let app = makeApp(environment: environment)
            let runtime = SteamClientRuntimeViewModel(app: app)
            XCTAssertEqual(all.filter { runtime.canPerform($0) }, [.install], "before the status is read only Install is offered (\(state))")
            await runtime.load()
            XCTAssertEqual(all.filter { runtime.canPerform($0) }, enabled, state)
            let busy = Task { await runtime.perform(.install) }
            try await waitUntil { runtime.job.value != nil }
            XCTAssertTrue(all.filter { runtime.canPerform($0) }.isEmpty, "nothing while a job runs (\(state))")
            await busy.value
            await app.shutdown()
        }
    }

    func testACancelledJobStartLeavesNothingLoading() async throws {
        // `AppModel.runJob` reports nothing on a cancellation, so the models
        // fall back to idle themselves; a stuck `.loading` would disable
        // every button for good.
        let app = makeApp(environment: ["FAKE_CORE_JOB_DELAY_MS": "400"])
        let runtime = SteamClientRuntimeViewModel(app: app)
        let install = Task { await runtime.perform(.install) }
        try await waitUntil { runtime.job == .loading }
        install.cancel()
        await install.value
        XCTAssertEqual(runtime.job, .idle)
        XCTAssertFalse(runtime.isBusy)

        let snapshots = PrefixSnapshotsViewModel(app: app, prefix: "steam-client")
        let reset = Task { await snapshots.resetPrefix(keepUserData: true) }
        try await waitUntil { snapshots.reset == .loading }
        reset.cancel()
        await reset.value
        XCTAssertEqual(snapshots.reset, .idle)
        XCTAssertFalse(snapshots.isBusy)
        await app.shutdown()
    }

    // MARK: - Snapshots

    func testSnapshotRestoreAndResetOnThePrefix() async throws {
        let app = makeApp()
        let snapshots = PrefixSnapshotsViewModel(app: app, prefix: "steam-client")
        await snapshots.takeSnapshot()
        XCTAssertEqual(snapshots.snapshot.value?.timestamp, "20261103T120001Z")
        XCTAssertEqual(snapshots.knownTimestamps, ["20261103T120001Z"])
        XCTAssertTrue(try request(for: "prefix.snapshot").contains("\"params\":{\"prefix\":\"steam-client\"}"))

        snapshots.restoreTimestamp = " 20261103T120001Z "
        await snapshots.restoreSelected()
        XCTAssertEqual(snapshots.restore.value?.timestamp, "20261103T130000Z", "the answer is the safety snapshot of the replaced state")
        XCTAssertTrue(try request(for: "prefix.restore").contains("\"params\":{\"prefix\":\"steam-client\",\"timestamp\":\"20261103T120001Z\"}"))
        XCTAssertEqual(snapshots.knownTimestamps, ["20261103T130000Z", "20261103T120001Z"])
        snapshots.restoreTimestamp = ""
        await snapshots.restoreSelected()
        XCTAssertEqual(try requests(for: "prefix.restore").count, 1, "an empty timestamp sends nothing")
        snapshots.restoreTimestamp = "garbage"
        await snapshots.restoreSelected()
        guard case .failed(.rpc(let refused)) = snapshots.restore else {
            return XCTFail("a name the core never made is refused, got \(snapshots.restore)")
        }
        XCTAssertEqual(refused.message, "snapshot steam-client/garbage does not exist")
        XCTAssertEqual(snapshots.knownTimestamps, ["20261103T130000Z", "20261103T120001Z"], "a refusal adds nothing")

        await snapshots.resetPrefix(keepUserData: false)
        XCTAssertEqual(snapshots.reset.value?.finished, true)
        XCTAssertTrue(try request(for: "prefix.reset").contains("\"params\":{\"keep_user_data\":false,\"prefix\":\"steam-client\"}"))
        XCTAssertEqual(snapshots.knownTimestamps.first, "20261103T140000Z", "the safety snapshot the reset reported")
        XCTAssertNil(snapshots.resetFailureSnapshot)
        await app.shutdown()
    }

    func testAFailedResetNamesTheSnapshotToRestore() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RESET_OUTCOME": "failed"])
        let detail = app.detail(for: 220)
        XCTAssertEqual(detail.snapshots.prefix, "220")
        await detail.snapshots.resetPrefix(keepUserData: true)
        guard case .failed(.rpc(let error)) = detail.snapshots.reset else {
            return XCTFail("expected the reset to fail, got \(detail.snapshots.reset)")
        }
        // `ResetFailed` carries its cause's code and text: a wineboot that
        // fails inside the rebuild is `WineCommandFailed`, Internal (1003).
        XCTAssertEqual(error.code, RpcErrorCode.internal.rawValue)
        XCTAssertTrue(error.message.hasPrefix("resetting prefix 220 failed; `applejack prefix restore 220 20261103T140000Z` brings it back: `"), error.message)
        XCTAssertTrue(error.message.hasSuffix("wineboot --init` failed (exit status: 1); log at `/data/logs/prefixes/220/wineboot.log`"), error.message)
        XCTAssertEqual(detail.snapshots.resetFailureSnapshot, "20261103T140000Z")
        XCTAssertEqual(detail.snapshots.knownTimestamps, ["20261103T140000Z"])
        await detail.snapshots.restore(timestamp: "20261103T140000Z")
        XCTAssertTrue(try request(for: "prefix.restore").contains("\"prefix\":\"220\",\"timestamp\":\"20261103T140000Z\""))
        await app.shutdown()
    }

    /// The core names a second snapshot of one prefix in the same second
    /// `<timestamp>-001` (`snapshot.rs`, `publish`); the shell must offer
    /// that name, not the one three characters shorter.
    func testASuffixedSnapshotNameIsReadWhole() async throws {
        let failing = makeApp(environment: ["FAKE_CORE_RESET_OUTCOME": "failed", "FAKE_CORE_RESET_SNAPSHOT": "20261103T140000Z-001"])
        let snapshots = PrefixSnapshotsViewModel(app: failing, prefix: "220")
        await snapshots.resetPrefix(keepUserData: true)
        XCTAssertEqual(snapshots.resetFailureSnapshot, "20261103T140000Z-001")
        XCTAssertEqual(snapshots.knownTimestamps, ["20261103T140000Z-001"])
        await snapshots.restore(timestamp: try XCTUnwrap(snapshots.resetFailureSnapshot))
        XCTAssertTrue(try request(for: "prefix.restore").contains("\"timestamp\":\"20261103T140000Z-001\""))
        await failing.shutdown()

        let app = makeApp(environment: ["FAKE_CORE_RESET_SNAPSHOT": "20261103T140000Z-002"])
        let reported = PrefixSnapshotsViewModel(app: app, prefix: "220")
        await reported.resetPrefix(keepUserData: false)
        XCTAssertEqual(reported.reset.value?.finished, true)
        XCTAssertEqual(reported.knownTimestamps, ["20261103T140000Z-002"], "the progress line's safety snapshot, whole")
        await app.shutdown()
    }

    // MARK: - QR polling

    func testPollingStopsWhenTheViewGoesAwayAndOnAnError() async throws {
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_POLLS": "100"])
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(30))
        await steam.startQrLogin()
        try await waitUntil { (try? self.requests(for: "steam.login.complete").count) ?? 0 >= 2 }
        steam.stopPolling()
        XCTAssertFalse(steam.isPolling)
        await fence(app)
        let sent = try requests(for: "steam.login.complete").count
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(try requests(for: "steam.login.complete").count, sent, "a stopped loop sends nothing more")
        guard case .ready(let pending) = steam.login, !pending.step.isDone else {
            return XCTFail("the login stays for the view that comes back: \(steam.login)")
        }
        steam.startPolling()
        try await waitUntil { (try? self.requests(for: "steam.login.complete").count) ?? 0 > sent }
        steam.stopPolling()
        await steam.waitForLogin()
        await app.shutdown()

        let failing = makeApp(environment: ["FAKE_CORE_LOGIN_FAIL": "1", "FAKE_CORE_REQUEST_LOG": secondRequestLog.path])
        let other = SteamAccountViewModel(app: failing, pollDelay: .milliseconds(30))
        await other.startQrLogin()
        await other.waitForLogin()
        XCTAssertEqual(other.login, .failed(.rpc(RpcError(code: RpcErrorCode.steamLoginFailed.rawValue, message: "the login is over; begin again"))))
        XCTAssertFalse(other.isPolling)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(try requests(for: "steam.login.complete", in: secondRequestLog).count, 1, "an error ends the loop")
        await failing.shutdown()
    }

    func testGuardCodeIsWantedUnlessTheFactorIsDeviceConfirmation() async throws {
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_GUARD": "device_confirmation", "FAKE_CORE_LOGIN_POLLS": "100"])
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(30))
        await steam.startQrLogin()
        XCTAssertFalse(steam.guardCodeWanted)
        XCTAssertTrue(steam.isPolling, "approval in the mobile app ends the login, so the poll goes on")
        steam.stopPolling()
        await steam.waitForLogin()
        await app.shutdown()

        let typed = makeApp(environment: ["FAKE_CORE_LOGIN_GUARD": "email_code", "FAKE_CORE_LOGIN_POLLS": "100", "FAKE_CORE_REQUEST_LOG": secondRequestLog.path])
        let other = SteamAccountViewModel(app: typed, pollDelay: .seconds(5))
        await other.startQrLogin()
        XCTAssertTrue(other.guardCodeWanted)
        // The loop's first poll goes out without a code; the submit does not wait 5 s for its tick.
        try await waitUntil { (try? self.requests(for: "steam.login.complete", in: self.secondRequestLog).contains { $0.contains("\"params\":{\"session_id\":\"login-session-fake\"}") }) ?? false }
        other.guardCode = "ABCDE"
        other.submitGuardCode()
        try await waitUntil({ (try? self.requests(for: "steam.login.complete", in: self.secondRequestLog).contains { $0.contains("ABCDE") }) ?? false }, within: .seconds(2))
        let sent = try requests(for: "steam.login.complete", in: secondRequestLog)
        XCTAssertTrue(sent.contains { $0.contains("\"steam_guard_code\":\"ABCDE\"") }, "\(sent)")
        try await waitUntil { other.guardCode.isEmpty }
        other.stopPolling()
        await other.waitForLogin()
        await typed.shutdown()
    }

    func testAWrongGuardCodeKeepsTheLoginOpen() async throws {
        // `docs/rpc-protocol.md`: a wrong code answers SteamGuardRejected and
        // the login stays open; the fake rejects the first code it sees.
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_GUARD": "device_code", "FAKE_CORE_LOGIN_POLLS": "100", "FAKE_CORE_LOGIN_REJECT_ONCE": "1"])
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(30))
        await steam.startQrLogin()
        XCTAssertTrue(steam.guardCodeWanted)
        XCTAssertNil(steam.guardRejection)
        steam.guardCode = "WRONG"
        steam.submitGuardCode()
        try await waitUntil { steam.guardRejection != nil }
        XCTAssertEqual(steam.guardRejection, "Steam Guard code rejected", "AuthError::InvalidGuardCode's text, as the core sends it")
        guard case .ready(let session) = steam.login, case .guardRequired(let factor) = session.step else {
            return XCTFail("the login stays open for another code, got \(steam.login)")
        }
        XCTAssertEqual(factor, "device_code")
        XCTAssertTrue(steam.guardCodeWanted, "the code field stays")
        XCTAssertTrue(steam.isPolling, "the poll goes on")
        XCTAssertTrue(steam.guardCode.isEmpty, "the rejected code is not sent again")
        let before = try requests(for: "steam.login.complete").count
        try await waitUntil { (try? self.requests(for: "steam.login.complete").count) ?? 0 > before }
        XCTAssertEqual(try requests(for: "steam.login.complete").filter { $0.contains("WRONG") }.count, 1)

        steam.guardCode = "RIGHT"
        steam.submitGuardCode()
        await steam.waitForLogin()
        XCTAssertEqual(steam.signedInAccount, "fake-account")
        XCTAssertNil(steam.guardRejection, "a code that went through clears the message")
        XCTAssertFalse(steam.isPolling)
        await app.shutdown()
    }

    func testGuardPromptsNameTheFactor() {
        XCTAssertEqual(SteamAccountViewModel.guardPrompt(factor: "email_code"), "Steam Guard: type the code from your email.")
        XCTAssertTrue(SteamAccountViewModel.guardPrompt(factor: "device_code").contains("or approve the sign-in there"))
        XCTAssertFalse(SteamAccountViewModel.guardPrompt(factor: "email_code").contains("approve"), "only the mobile app can approve instead of a code")
        XCTAssertEqual(SteamAccountViewModel.guardPrompt(factor: "device_confirmation"), "Steam Guard: approve the sign-in in the Steam mobile app.")
        XCTAssertTrue(SteamAccountViewModel.guardPrompt(factor: "new_factor").contains("new_factor"))
    }

    func testADroppedAccountModelStopsPolling() async throws {
        // Round-three review S3: the loop used to hold the model through
        // its sleep, so a dropped model polled on until `done`.
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_POLLS": "100"])
        weak var weakModel: SteamAccountViewModel?
        do {
            let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(30))
            weakModel = steam
            await steam.startQrLogin()
            try await waitUntil { (try? self.requests(for: "steam.login.complete").count) ?? 0 >= 2 }
        }
        try await waitUntil { weakModel == nil }
        // The model can only die in the loop's sleep (`tick` holds it), so the
        // last poll is answered and logged; the fence guards the count against
        // a fake that logs late, not against a poll in flight.
        await fence(app)
        let sent = try requests(for: "steam.login.complete").count
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(try requests(for: "steam.login.complete").count, sent, "a dropped model sends nothing more")
        await app.shutdown()
    }

    // MARK: - Engines and jobs

    func testEngineInstallStreamsProgressToDone() async throws {
        let app = makeApp()
        let engines = EnginesViewModel(app: app)
        var seen: [FlowState<JobSnapshot>] = []
        await app.runJob("engine.install", params: EngineInstallParams(engineId: "rosetta-wine")) { seen.append($0) }
        XCTAssertEqual(seen.first, .loading)
        let snapshots = seen.compactMap(\.value)
        XCTAssertEqual(snapshots.map(\.phase), ["", "download", "extract", "extract"])
        XCTAssertEqual(snapshots[1].fraction, 0.25)
        XCTAssertEqual(snapshots[2].message, "unpacking")
        XCTAssertEqual(snapshots.map(\.finished), [false, false, false, true])
        XCTAssertTrue(snapshots.allSatisfy { $0.jobId == JobId(rawValue: 7) })

        await engines.install("rosetta-wine")
        XCTAssertEqual(engines.install.value?.finished, true)
        XCTAssertEqual(try requests(for: "engine.list").count, 1, "a finished install reloads the list")
        await app.shutdown()
    }

    func testEngineInstallFailureCarriesTheTypedCode() async throws {
        let app = makeApp(environment: ["FAKE_CORE_JOB_OUTCOME": "failed"])
        let engines = EnginesViewModel(app: app)
        await engines.install("rosetta-wine")
        XCTAssertEqual(engines.install, .failed(.rpc(RpcError(code: RpcErrorCode.jobFailed.rawValue, message: "checksum mismatch"))))
        await app.shutdown()
    }

    func testCoreDeathDuringAJobFailsTheFlowAndShowsTheBanner() async throws {
        let pidFile = makePidFile()
        defer { CoreClientTests.removePidFile(pidFile) }
        let app = makeApp(environment: ["FAKE_CORE_JOB_OUTCOME": "never", "FAKE_CORE_PID_FILE": pidFile.path])
        await app.start()
        let engines = EnginesViewModel(app: app)
        let install = Task { await engines.install("rosetta-wine") }
        try await waitUntil { engines.install.value != nil }
        let pid = try XCTUnwrap(Int32(try String(contentsOf: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)))
        kill(pid, SIGKILL)
        await install.value
        XCTAssertEqual(engines.install, .failed(.coreExited(9)))
        XCTAssertEqual(app.coreStatus, .exited(9))
        await app.shutdown()
    }

    func testEngineListUseDoctorAndProbe() async throws {
        let app = makeApp()
        let engines = EnginesViewModel(app: app)
        await engines.load()
        XCTAssertEqual(engines.engines.value?.engines.map(\.active), [true, false])
        await engines.use("rosetta-wine", version: "10.0.0")
        XCTAssertEqual(engines.use, .ready(.object([:])))
        XCTAssertTrue(try request(for: "engine.use").contains("\"params\":{\"engine_id\":\"rosetta-wine\",\"version\":\"10.0.0\"}"))
        await engines.runDoctor()
        XCTAssertEqual(engines.doctor.value?.runnable, true)
        XCTAssertEqual(engines.doctor.value?.findings.map(\.severity), [.info, .warning])
        await engines.probeCpuid()
        XCTAssertEqual(engines.cpuid.value, .object(["advertise_avx": .bool(true), "avx": .bool(true), "avx2": .bool(false), "osxsave": .bool(true)]))
        await app.shutdown()
    }

    // MARK: - Steam account

    func testQrLoginRendersTheChallengeAndCompletes() async throws {
        let app = makeApp()
        let steam = SteamAccountViewModel(app: app)
        await steam.startQrLogin()
        XCTAssertEqual(steam.qrPayload, "https://s.team/q/1/fake-challenge")
        XCTAssertTrue(try request(for: "steam.login.start").contains("\"params\":{\"mode\":{\"kind\":\"qr\"}}"))
        let image = try XCTUnwrap(QRCode.cgImage(for: try XCTUnwrap(steam.qrPayload)))
        XCTAssertGreaterThan(image.width, 0)

        XCTAssertTrue(steam.isPolling, "polling starts with the challenge, not with a button")
        await steam.waitForLogin()
        XCTAssertEqual(steam.signedInAccount, "fake-account")
        XCTAssertFalse(steam.isPolling)
        XCTAssertTrue(try request(for: "steam.login.complete").contains("\"params\":{\"session_id\":\"login-session-fake\"}"), "no guard code means no member")

        await steam.signOut()
        XCTAssertEqual(steam.login, .idle)
        XCTAssertEqual(steam.logout, .ready(.object([:])))
        await app.shutdown()
    }

    /// Red if the loop tests only for `.ready`: the tick after the sleep
    /// would send the old id for a login that has been replaced.
    func testANewLoginEndsThePollOfTheOldOne() async throws {
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_ROTATE": "1", "FAKE_CORE_LOGIN_POLLS": "3"])
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(20))
        await steam.startQrLogin()
        while try requests(for: "steam.login.complete").isEmpty {
            try await Task.sleep(for: .milliseconds(10))
        }
        await steam.startQrLogin()
        await steam.waitForLogin()

        let sent = try requests()
        let restart = try XCTUnwrap(sent.lastIndex { $0.contains("\"method\":\"steam.login.start\"") })
        XCTAssertFalse(sent[restart...].contains { $0.contains("login-session-fake-1") }, "\(sent)")
        guard case .ready(let session) = steam.login else { return XCTFail("\(steam.login)") }
        XCTAssertEqual(session.sessionId, "login-session-fake-2")
        XCTAssertEqual(steam.signedInAccount, "fake-account")
        await app.shutdown()
    }

    /// Red if the new login waited for the old loop's sleep: the first poll
    /// of the new session must go out at once.
    func testANewLoginDuringTheOldLoopsSleepPollsAtOnce() async throws {
        let app = makeApp(environment: ["FAKE_CORE_LOGIN_ROTATE": "1", "FAKE_CORE_LOGIN_POLLS": "100"])
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(1500))
        await steam.startQrLogin()
        while try requests(for: "steam.login.complete").isEmpty {
            try await Task.sleep(for: .milliseconds(10))
        }
        await steam.startQrLogin()
        let deadline = ContinuousClock.now + .milliseconds(500)
        while try !requests(for: "steam.login.complete").contains(where: { $0.contains("login-session-fake-2") }) {
            if ContinuousClock.now > deadline { break }
            try await Task.sleep(for: .milliseconds(10))
        }
        let sent = try requests(for: "steam.login.complete")
        XCTAssertTrue(sent.contains { $0.contains("login-session-fake-2") }, "\(sent)")
        steam.stopPolling()
        await steam.waitForLogin()
        await app.shutdown()
    }

    func testLoginPollsUntilSteamIssuesTheTokens() async throws {
        setenv("FAKE_CORE_LOGIN_POLLS", "1", 1)
        defer { unsetenv("FAKE_CORE_LOGIN_POLLS") }
        let app = makeApp()
        let steam = SteamAccountViewModel(app: app, pollDelay: .milliseconds(20))
        await steam.startQrLogin()
        await steam.waitForLogin()
        XCTAssertEqual(steam.signedInAccount, "fake-account")
        XCTAssertEqual(try requests(for: "steam.login.complete").count, 2)
        await app.shutdown()
    }

    func testQrImageHasDarkAndLightPixels() throws {
        let image = try XCTUnwrap(QRCode.cgImage(for: "https://s.team/q/1/abcdef0123456789", scale: 4))
        XCTAssertGreaterThan(image.width, 100)
        XCTAssertEqual(image.width, image.height)
        let count = image.width * image.height
        var pixels = [UInt8](repeating: 0, count: count)
        let context = try XCTUnwrap(CGContext(
            data: &pixels, width: image.width, height: image.height, bitsPerComponent: 8,
            bytesPerRow: image.width, space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue
        ))
        context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
        let dark = pixels.filter { $0 < 128 }.count
        XCTAssertGreaterThan(dark, count / 10)
        XCTAssertLessThan(dark, count * 9 / 10)
        XCTAssertNil(QRCode.cgImage(for: ""))
    }

    // MARK: - D3DMetal

    /// A directory standing in for a mounted Game Porting Toolkit image,
    /// with `License.pdf` unless `licensed` is false.
    func makeVolume(licensed: Bool = true) throws -> URL {
        let volume = FileManager.default.temporaryDirectory.appendingPathComponent("gptk-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: volume, withIntermediateDirectories: true)
        if licensed {
            try Data("%PDF-1.4\n".utf8).write(to: volume.appendingPathComponent("License.pdf"))
        }
        return volume
    }

    func testD3DMetalImportIsNeverSentWithoutTheAcknowledgement() async throws {
        let volume = try makeVolume()
        defer { try? FileManager.default.removeItem(at: volume) }
        let app = makeApp()
        let model = D3DMetalImportViewModel(app: app)
        await model.requestImport()
        XCTAssertFalse(model.showingLicense, "no volume, nothing to acknowledge")
        model.chooseVolume(volume.path)
        XCTAssertTrue(model.licenseFound)
        await model.requestImport()
        XCTAssertTrue(model.showingLicense)
        XCTAssertEqual(model.importState, .idle)
        model.declineLicense()
        await model.requestImport()
        XCTAssertTrue(model.showingLicense)
        _ = await app.run { try await $0.hostDoctor() }
        XCTAssertTrue(try requests(for: "engine.d3dmetal.import").isEmpty, "declined twice: the import must not have been requested")

        await model.acknowledgeLicense()
        XCTAssertFalse(model.showingLicense)
        XCTAssertEqual(model.importState, .ready(.object([:])))
        let imports = try requests(for: "engine.d3dmetal.import")
        XCTAssertEqual(imports.count, 1)
        XCTAssertTrue(try XCTUnwrap(imports.first).contains("\"params\":{\"license_acknowledged\":true,\"path\":\"\(volume.path)\"}"), "\(imports)")
        XCTAssertEqual(model.licenseURL?.path, volume.appendingPathComponent("License.pdf").path)
        await app.shutdown()
    }

    func testTheLicenseGateNeedsTheFileOnTheVolume() async throws {
        let volume = try makeVolume(licensed: false)
        defer { try? FileManager.default.removeItem(at: volume) }
        let app = makeApp()
        let model = D3DMetalImportViewModel(app: app)
        model.chooseVolume(volume.path)
        XCTAssertFalse(model.licenseFound)
        await model.requestImport()
        XCTAssertTrue(model.showingLicense)
        await model.acknowledgeLicense()
        XCTAssertTrue(model.showingLicense, "nothing to accept without License.pdf")
        XCTAssertFalse(model.licenseAcknowledged)
        _ = await app.run { try await $0.hostDoctor() }
        XCTAssertTrue(try requests(for: "engine.d3dmetal.import").isEmpty)

        // The image is mounted while the sheet is open: the sheet's poll
        // calls `refreshLicense()`, and the next acceptance goes through.
        try Data("%PDF-1.4\n".utf8).write(to: volume.appendingPathComponent("License.pdf"))
        model.refreshLicense()
        XCTAssertTrue(model.licenseFound)
        await model.acknowledgeLicense()
        XCTAssertFalse(model.showingLicense)
        XCTAssertEqual(try requests(for: "engine.d3dmetal.import").count, 1)
        await app.shutdown()
    }

    func testEachD3DMetalImportNeedsItsOwnAcknowledgement() async throws {
        let volume = try makeVolume()
        defer { try? FileManager.default.removeItem(at: volume) }
        let app = makeApp()
        let model = D3DMetalImportViewModel(app: app)
        model.chooseVolume(volume.path)
        await model.requestImport()
        await model.acknowledgeLicense()
        XCTAssertEqual(try requests(for: "engine.d3dmetal.import").count, 1)
        XCTAssertFalse(model.licenseAcknowledged, "the import consumed the acknowledgement")

        // A second import asks again and sends nothing until accepted.
        await model.requestImport()
        XCTAssertTrue(model.showingLicense)
        _ = await app.run { try await $0.hostDoctor() }
        XCTAssertEqual(try requests(for: "engine.d3dmetal.import").count, 1)

        // Choosing another volume drops an acknowledgement given for the last one.
        model.declineLicense()
        model.licenseAcknowledged = true
        model.chooseVolume(volume.path)
        XCTAssertTrue(model.licenseAcknowledged, "the same volume keeps it")
        model.chooseVolume("/Volumes/GPTK 4")
        XCTAssertFalse(model.licenseAcknowledged)
        await model.requestImport()
        XCTAssertTrue(model.showingLicense)
        _ = await app.run { try await $0.hostDoctor() }
        XCTAssertEqual(try requests(for: "engine.d3dmetal.import").count, 1)
        await app.shutdown()
    }

    // MARK: - Logs

    func testLogsListsSessionsAndTailsTheLog() async throws {
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("fake-session-\(UUID().uuidString).log")
        defer { try? FileManager.default.removeItem(at: log) }
        let app = makeApp(environment: ["FAKE_CORE_SESSION_LOG": log.path])
        let logs = LogsViewModel(app: app)
        await logs.load(appid: 220)
        let session = try XCTUnwrap(logs.sessions.value?.sessions.first)
        XCTAssertEqual(session.logPath, log.path)
        XCTAssertTrue(try request(for: "session.list").contains("\"params\":{\"appid\":220}"))

        // The file does not exist yet when the tail starts.
        logs.select(session)
        XCTAssertEqual(logs.tailedPath, log.path)
        try Data("first\nsecond\n".utf8).write(to: log)
        try await waitUntil { logs.lines.count == 2 }
        let handle = try FileHandle(forWritingTo: log)
        try handle.seekToEnd()
        try handle.write(contentsOf: Data("third\n".utf8))
        try handle.close()
        try await waitUntil { logs.lines.count == 3 }
        XCTAssertEqual(logs.lines, ["first", "second", "third"])

        // Leaving the view suspends the poll; coming back re-reads the file.
        logs.suspend()
        XCTAssertEqual(logs.tailedPath, log.path)
        logs.resume()
        try await waitUntil { logs.lines.count == 3 }
        XCTAssertEqual(logs.lines, ["first", "second", "third"])
        logs.stop()
        XCTAssertTrue(logs.lines.isEmpty)
        XCTAssertNil(logs.tailedPath)
        await app.shutdown()
    }

    func testACancelledTailAddsNothingAndAResumedOneStartsClean() async throws {
        // Regression for the round-two review (probes PH, PH2, PH3): the
        // tailer yields a whole file into the stream at once, and lines the
        // consumer had not reached when `suspend()`, `stop()`, or a new
        // `follow` cancelled it used to land in the next tail's `lines`.
        let a = FileManager.default.temporaryDirectory.appendingPathComponent("tail-a-\(UUID().uuidString).log")
        let b = FileManager.default.temporaryDirectory.appendingPathComponent("tail-b-\(UUID().uuidString).log")
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
        }
        let expected = (0..<1500).map { "line\($0)" }
        try Data((expected.joined(separator: "\n") + "\n").utf8).write(to: a)
        try Data("B0\nB1\n".utf8).write(to: b)
        let logs = LogsViewModel(app: AppModel(client: nil))

        logs.follow(a.path)
        while logs.lines.count < 50 { await Task.yield() }
        logs.suspend()
        let atSuspend = logs.lines.count
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(logs.lines.count, atSuspend, "a suspended tail appends nothing")
        logs.resume()
        try await waitUntil { logs.lines.count == expected.count }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(logs.lines, expected, "resume re-reads the file once, in order")

        // Hide and show in the same turn, mid-consumption.
        logs.stop()
        logs.follow(a.path)
        while logs.lines.count < 50 { await Task.yield() }
        logs.suspend()
        logs.resume()
        try await waitUntil { logs.lines.count == expected.count }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(logs.lines, expected)

        // Following another file mid-consumption shows only that file.
        logs.follow(a.path)
        while logs.lines.count < 50 { await Task.yield() }
        logs.follow(b.path)
        try await waitUntil { logs.lines.count == 2 }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(logs.lines, ["B0", "B1"])
        logs.stop()
    }

    func testLogTailerStartsOverWhenTheFileIsReplacedByALongerOne() async throws {
        // Probe PI of the round-two review read ["a", "23456789", "next"]:
        // a replacement at least as long as what was read passed the
        // size-only rotation check.
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("replaced-\(UUID().uuidString).log")
        defer { try? FileManager.default.removeItem(at: log) }
        try Data("a\n".utf8).write(to: log)
        let tail = CollectedLines(LogTailer.lines(at: log.path, interval: .milliseconds(20)))
        try await waitUntil { tail.lines.count == 1 }
        try FileManager.default.removeItem(at: log)
        try Data("0123456789\nnext\n".utf8).write(to: log)
        try await waitUntil { tail.lines.count >= 3 }
        await tail.stop()
        XCTAssertEqual(tail.lines, ["a", "0123456789", "next"])
    }

    func testLogTailerStartsOverAfterRotation() async throws {
        // Lifted from the review of 747fac1 (probe P4), which read
        // ["one", "two", "three", "", "new4"] from this sequence.
        let log = FileManager.default.temporaryDirectory.appendingPathComponent("rotated-\(UUID().uuidString).log")
        defer { try? FileManager.default.removeItem(at: log) }
        try Data("one\ntwo\nthree\n".utf8).write(to: log)
        let tail = CollectedLines(LogTailer.lines(at: log.path, interval: .milliseconds(20)))
        try await waitUntil { tail.lines.count == 3 }
        try FileManager.default.removeItem(at: log)
        try Data("new1\n".utf8).write(to: log)
        try await waitUntil { tail.lines.count >= 4 }
        // Appends as a log writer does: rewriting in place truncates first, and
        // a poll in that window rightly starts the file over.
        let handle = try FileHandle(forWritingTo: log)
        try handle.seekToEnd()
        try handle.write(contentsOf: Data("new2\nnew3\nnew4\n".utf8))
        try handle.close()
        try await waitUntil { tail.lines.count >= 7 }
        await tail.stop()
        XCTAssertEqual(tail.lines, ["one", "two", "three", "new1", "new2", "new3", "new4"])
    }

    // MARK: - Controllers

    func testControllersComeFromTheProvider() {
        let pad = ControllerStatus(id: "0:Sony", vendorName: "DualSense", productCategory: "DualSense", hasExtendedGamepad: true, isAttachedToDevice: false)
        let model = ControllersViewModel(provider: { [pad] })
        XCTAssertTrue(model.controllers.isEmpty)
        model.refresh()
        XCTAssertEqual(model.controllers, [pad])
        // The real provider runs without a pad attached and lists nothing or whatever is connected.
        _ = ControllersViewModel.connectedControllers()
    }

    // MARK: - Core lifecycle

    func testCoreLocatorPrefersTheEnvironmentThenTheSibling() {
        let executable = URL(fileURLWithPath: "/Applications/Applejack.app/Contents/MacOS/Applejack")
        let sibling = URL(fileURLWithPath: "/Applications/Applejack.app/Contents/MacOS/applejack-core")
        let env = CoreLocator(environment: ["APPLEJACK_CORE_BIN": "/dev/core"], executableURL: executable, isExecutable: { _ in true })
        XCTAssertEqual(env.locate(), URL(fileURLWithPath: "/dev/core"))
        let bundled = CoreLocator(environment: [:], executableURL: executable, isExecutable: { $0 == sibling })
        XCTAssertEqual(bundled.locate(), sibling)
        let missing = CoreLocator(environment: ["APPLEJACK_CORE_BIN": ""], executableURL: executable, isExecutable: { _ in false })
        XCTAssertNil(missing.locate())
        let none = AppModel(locator: missing)
        XCTAssertEqual(none.coreStatus, .missing)
        XCTAssertNil(none.client)
        // A set variable is used as given; whether it runs is the spawn's call.
        let unusable = CoreLocator(environment: ["APPLEJACK_CORE_BIN": "/nonexistent/core"], executableURL: executable, isExecutable: { _ in false })
        XCTAssertEqual(unusable.locate(), URL(fileURLWithPath: "/nonexistent/core"))
    }

    func testAnUnsetCoreBinIsMissingButASetUnusableOneIsStoppedWithRestart() async throws {
        // Round-two review B-I1: `APPLEJACK_CORE_BIN=/nonexistent` used to
        // read "is not set" with no Restart.
        let unset = AppModel(locator: CoreLocator(environment: [:], executableURL: nil))
        await unset.start()
        XCTAssertEqual(unset.coreStatus, .missing)
        XCTAssertNil(unset.client)
        await unset.shutdown()

        let bad = FileManager.default.temporaryDirectory.appendingPathComponent("no-such-core-\(UUID().uuidString)").path
        let app = AppModel(locator: CoreLocator(environment: ["APPLEJACK_CORE_BIN": bad], executableURL: nil))
        XCTAssertNotNil(app.client)
        XCTAssertEqual(app.coreLocation, URL(fileURLWithPath: bad))
        XCTAssertEqual(app.coreStatus, .starting)
        await app.start()
        guard case .stopped(.launchFailed(let detail)) = app.coreStatus else {
            return XCTFail("expected stopped(launchFailed), got \(app.coreStatus)")
        }
        XCTAssertTrue(detail.contains(bad), "the banner names the path: \(detail)")
        let host = HostViewModel(app: app)
        await host.load()
        guard case .failed(.launchFailed) = host.facts else { return XCTFail("expected launchFailed, got \(host.facts)") }

        // Restart after the file is fixed recovers without a relaunch.
        await app.restart()
        guard case .stopped(.launchFailed) = app.coreStatus else { return XCTFail("still no file: \(app.coreStatus)") }
        try FileManager.default.copyItem(at: CoreClientTests.fakeCore, to: URL(fileURLWithPath: bad))
        defer { try? FileManager.default.removeItem(atPath: bad) }
        await app.restart()
        guard case .running = app.coreStatus else { return XCTFail("expected running after the fix, got \(app.coreStatus)") }
        let restarts = await app.client?.restarts
        XCTAssertEqual(restarts, 0, "a launch failure spends no restart")
        await app.shutdown()
    }

    func testAHelloThatCannotBeReadIsStoppedWithRestart() async throws {
        // Round-two review A-S1: the banner stayed on "Starting the core…".
        for (knob, expectation) in [("malformed", "malformedResponse"), ("error", "rpc")] {
            let app = makeApp(environment: ["FAKE_CORE_BAD_HELLO": knob])
            await app.start()
            guard case .stopped(let error) = app.coreStatus else {
                return XCTFail("\(knob): expected stopped, got \(app.coreStatus)")
            }
            switch (expectation, error) {
            case ("malformedResponse", .malformedResponse(let detail)):
                // The banner names the binary and says what was wrong in one line.
                XCTAssertTrue(detail.hasPrefix(CoreClientTests.fakeCore.path + ": "), detail)
                XCTAssertFalse(detail.contains("DecodingError"), detail)
                XCTAssertFalse(detail.contains("\n"), detail)
                XCTAssertTrue(detail.contains("`protocol_version` is missing"), detail)
            case ("rpc", .rpc): break
            default: XCTFail("\(knob): expected \(expectation), got \(error)")
            }
            await app.restart()
            guard case .stopped = app.coreStatus else { return XCTFail("\(knob): expected stopped after Restart, got \(app.coreStatus)") }
            // A flow's own rpc error still never reaches the banner.
            await app.shutdown()
        }
    }

    func testADroppedModelReleasesItsClient() async throws {
        // Round-two review A-S2 (probe PM): the exit observer held the
        // client until the next event, which an idle core never sends.
        weak var weakClient: CoreClient?
        weak var weakModel: AppModel?
        do {
            let client = CoreClient(executable: CoreClientTests.fakeCore, arguments: [], environment: [:])
            weakClient = client
            let app = AppModel(client: client)
            weakModel = app
            await app.start()
            guard case .running = app.coreStatus else { return XCTFail("expected running, got \(app.coreStatus)") }
        }
        try await waitUntil { weakModel == nil && weakClient == nil }
    }

    func testBannerRestartReloadsTheSelectedSection() async throws {
        let app = makeApp(environment: ["FAKE_CORE_CRASH_AFTER": "2"])
        let models = ShellModels(app: app, initialSection: .host)
        await app.start()
        await models.host.load()
        await models.host.load()
        XCTAssertEqual(models.host.facts, .failed(.coreExited(1)))
        XCTAssertEqual(app.coreStatus, .exited(1))
        await app.restart()
        guard case .running = app.coreStatus else { return XCTFail("expected running, got \(app.coreStatus)") }
        await models.reloadSection()
        XCTAssertEqual(models.host.facts.value?.arch, "arm64", "the view's failed state clears with the banner")
        await app.shutdown()
    }

    func testCoreExitShowsTheBannerAndRestartRecovers() async throws {
        let app = makeApp(environment: ["FAKE_CORE_CRASH_AFTER": "2"])
        await app.start()
        guard case .running(let hello) = app.coreStatus else { return XCTFail("expected running, got \(app.coreStatus)") }
        XCTAssertEqual(hello.coreVersion, "fake")
        let host = HostViewModel(app: app)
        await host.load()
        XCTAssertEqual(host.facts.value?.arch, "arm64")
        await host.load()
        XCTAssertEqual(host.facts, .failed(.coreExited(1)))
        XCTAssertEqual(app.coreStatus, .exited(1))

        await app.restart()
        guard case .running = app.coreStatus else { return XCTFail("expected running after restart, got \(app.coreStatus)") }
        let restarts = await app.client?.restarts
        XCTAssertEqual(restarts, 1)
        await host.load()
        XCTAssertEqual(host.facts.value?.arch, "arm64")
        await app.shutdown()
    }

    func testAStoppedCoreIsRetryableAndAHealthyCallClearsTheBanner() async throws {
        let app = makeApp()
        await app.start()
        let host = HostViewModel(app: app)
        app.note(.notRunning)
        XCTAssertEqual(app.coreStatus, .stopped(.notRunning))
        app.note(.launchFailed("spawn"))
        XCTAssertEqual(app.coreStatus, .stopped(.launchFailed("spawn")))
        app.note(.handshakeTimedOut)
        XCTAssertEqual(app.coreStatus, .stopped(.handshakeTimedOut))
        // The next call respawns lazily; a success proves the core is up.
        await host.load()
        XCTAssertEqual(host.facts.value?.arch, "arm64")
        guard case .running = app.coreStatus else { return XCTFail("expected running, got \(app.coreStatus)") }

        app.note(.restartLimit)
        XCTAssertEqual(app.coreStatus, .failed(.restartLimit))
        app.note(.rpc(RpcError(code: RpcErrorCode.internal.rawValue, message: "boom")))
        XCTAssertEqual(app.coreStatus, .failed(.restartLimit), "an rpc error never touches the banner")
        await app.shutdown()
    }

    func testACallAfterAnExitClearsTheBannerWithoutRestart() async throws {
        let app = makeApp(environment: ["FAKE_CORE_CRASH_AFTER": "2"])
        await app.start()
        let host = HostViewModel(app: app)
        await host.load()
        await host.load()
        XCTAssertEqual(app.coreStatus, .exited(1))
        await host.load()
        XCTAssertEqual(host.facts.value?.arch, "arm64")
        guard case .running = app.coreStatus else { return XCTFail("expected running after the lazy respawn, got \(app.coreStatus)") }
        await app.shutdown()
    }

    func testAnIdleCoreExitReachesTheBannerThroughEvents() async throws {
        let pidFile = makePidFile()
        defer { CoreClientTests.removePidFile(pidFile) }
        let app = makeApp(environment: ["FAKE_CORE_PID_FILE": pidFile.path])
        await app.start()
        let pid = try XCTUnwrap(Int32(try String(contentsOf: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)))
        kill(pid, SIGKILL)
        try await waitUntil { app.coreStatus == .exited(9) }
        await app.shutdown()
    }

    /// Everything the shell wrote before this call is in the fake's log once
    /// it answers: the fake reads its pipe in order and logs each line before
    /// it acts on it. The log is the fake's view, behind the pipe, so a count
    /// taken at the instant of a stop can miss the poll already written; a
    /// count taken after the fence is exact, and a request the stop left
    /// behind lands after it.
    private func fence(_ app: AppModel, file: StaticString = #filePath, line: UInt = #line) async {
        let answer = await app.run { try await $0.call("engine.list", params: EmptyParams()) as JSONValue }
        XCTAssertNotNil(answer?.value, "the fence needs an answer from the fake, or it fenced nothing: \(String(describing: answer))", file: file, line: line)
    }

    private func makePidFile() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-\(UUID().uuidString).pid")
    }

    private func waitUntil(_ condition: @MainActor () -> Bool, within limit: Duration = .seconds(5), file: StaticString = #filePath, line: UInt = #line) async throws {
        let deadline = ContinuousClock.now + limit
        while !condition(), ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(condition(), "condition not met within \(limit)", file: file, line: line)
    }
}

/// What a tailer has delivered so far, so a test can wait for a line rather
/// than guess how long a poll takes on a busy machine.
@MainActor
private final class CollectedLines {
    private(set) var lines: [String] = []
    private var reader: Task<Void, Never>?

    init(_ stream: AsyncStream<String>) {
        reader = Task { [weak self] in
            for await line in stream {
                self?.lines.append(line)
            }
        }
    }

    func stop() async {
        reader?.cancel()
        await reader?.value
    }
}
