import CoreGraphics
import Foundation
import XCTest

@testable import ApplejackShell

/// The view models against `Tests/Fixtures/fake-core.sh`. Without
/// `FAKE_CORE_IMPLEMENTED` the fake answers like today's core: every flow
/// method is `NotImplemented`.
@MainActor
final class ViewModelTests: XCTestCase {
    nonisolated let requestLog = FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-requests-\(UUID().uuidString).log")

    override func tearDown() {
        try? FileManager.default.removeItem(at: requestLog)
        super.tearDown()
    }

    func makeApp(implemented: Bool = true, environment: [String: String] = [:]) -> AppModel {
        var env = environment
        env["FAKE_CORE_REQUEST_LOG"] = requestLog.path
        if implemented {
            env["FAKE_CORE_IMPLEMENTED"] = "1"
        }
        return AppModel(client: CoreClient(executable: CoreClientTests.fakeCore, arguments: [], environment: env))
    }

    func requests() throws -> [String] {
        guard FileManager.default.fileExists(atPath: requestLog.path) else { return [] }
        return try String(contentsOf: requestLog, encoding: .utf8).split(separator: "\n").map(String.init)
    }

    func requests(for method: String) throws -> [String] {
        try requests().filter { $0.contains("\"method\":\"\(method)\"") }
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
        XCTAssertTrue(try requests(for: "steam.library")[0].contains("\"params\":{\"refresh\":true}"))

        await library.install(440)
        guard case .ready(let snapshot) = library.installs[440] else {
            return XCTFail("expected a finished install, got \(String(describing: library.installs[440]))")
        }
        XCTAssertTrue(snapshot.finished)
        XCTAssertEqual(snapshot.jobId, JobId(rawValue: 8))
        XCTAssertTrue(try requests(for: "app.install")[0].contains("\"params\":{\"appid\":440,\"update\":false,\"verify\":false}"))
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
        XCTAssertTrue(try requests(for: "config.set")[0].contains("\"params\":{\"appid\":220,\"key\":\"graphics_backend\",\"value\":\"wined3d\"}"))
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
        XCTAssertTrue(imports[0].contains("\"accept_launch_overrides\":false"))

        await detail.confirmLaunchOverrides()
        XCTAssertTrue(detail.pendingConfirmation.isEmpty)
        XCTAssertEqual(detail.importState.value?.applied, true)
        imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 2)
        XCTAssertTrue(imports[1].contains("\"accept_launch_overrides\":true"))
        XCTAssertEqual(try requests(for: "config.get").count, 1, "an applied import reloads the config")

        // Declining sends nothing.
        await detail.importPreset(at: "/tmp/220.preset.json")
        detail.declineLaunchOverrides()
        XCTAssertEqual(try requests(for: "config.import").count, 3)
        XCTAssertEqual(detail.importState, .idle)
        XCTAssertNil(detail.pendingImport)
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
        XCTAssertEqual(detail.importState, .idle)
        await detail.confirmLaunchOverrides(path: presented.path)
        XCTAssertEqual(detail.importState.value?.applied, true)
        let imports = try requests(for: "config.import")
        XCTAssertEqual(imports.count, 2)
        XCTAssertTrue(imports[1].contains("\"accept_launch_overrides\":true"), imports[1])
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
        XCTAssertEqual(detail.runState.value?.logPath, log.path)
        XCTAssertTrue(try requests(for: "app.run")[0].contains("\"params\":{\"appid\":220,\"probe_direct\":false}"))
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
        let pidFile = FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-\(UUID().uuidString).pid")
        defer { try? FileManager.default.removeItem(at: pidFile) }
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
        XCTAssertTrue(try requests(for: "engine.use")[0].contains("\"params\":{\"engine_id\":\"rosetta-wine\",\"version\":\"10.0.0\"}"))
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
        XCTAssertTrue(try requests(for: "steam.login.start")[0].contains("\"params\":{\"mode\":{\"kind\":\"qr\"}}"))
        let image = try XCTUnwrap(QRCode.cgImage(for: try XCTUnwrap(steam.qrPayload)))
        XCTAssertGreaterThan(image.width, 0)

        await steam.complete()
        XCTAssertEqual(steam.signedInAccount, "fake-account")
        XCTAssertTrue(try requests(for: "steam.login.complete")[0].contains("\"params\":{\"session_id\":\"login-session-fake\"}"), "no guard code means no member")

        await steam.signOut()
        XCTAssertEqual(steam.login, .idle)
        XCTAssertEqual(steam.logout, .ready(.object([:])))
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

    func testD3DMetalImportIsNeverSentWithoutTheAcknowledgement() async throws {
        let app = makeApp()
        let model = D3DMetalImportViewModel(app: app)
        await model.requestImport()
        XCTAssertFalse(model.showingLicense, "no volume, nothing to acknowledge")
        model.chooseVolume("/Volumes/Game Porting Toolkit")
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
        XCTAssertTrue(imports[0].contains("\"params\":{\"license_acknowledged\":true,\"path\":\"/Volumes/Game Porting Toolkit\"}"))
        XCTAssertEqual(model.licenseURL?.path, "/Volumes/Game Porting Toolkit/License.pdf")
        await app.shutdown()
    }

    func testEachD3DMetalImportNeedsItsOwnAcknowledgement() async throws {
        let app = makeApp()
        let model = D3DMetalImportViewModel(app: app)
        model.chooseVolume("/Volumes/Game Porting Toolkit")
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
        model.chooseVolume("/Volumes/Game Porting Toolkit")
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
        XCTAssertTrue(try requests(for: "session.list")[0].contains("\"params\":{\"appid\":220}"))

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
        logs.stop()
        XCTAssertTrue(logs.lines.isEmpty)
        await app.shutdown()
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

    func testAnIdleCoreExitReachesTheBannerThroughEvents() async throws {
        let pidFile = FileManager.default.temporaryDirectory.appendingPathComponent("fake-core-\(UUID().uuidString).pid")
        defer { try? FileManager.default.removeItem(at: pidFile) }
        let app = makeApp(environment: ["FAKE_CORE_PID_FILE": pidFile.path])
        await app.start()
        let pid = try XCTUnwrap(Int32(try String(contentsOf: pidFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines)))
        kill(pid, SIGKILL)
        try await waitUntil { app.coreStatus == .exited(9) }
        await app.shutdown()
    }

    private func waitUntil(_ condition: @MainActor () -> Bool, within limit: Duration = .seconds(5), file: StaticString = #filePath, line: UInt = #line) async throws {
        let deadline = ContinuousClock.now + limit
        while !condition(), ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(50))
        }
        XCTAssertTrue(condition(), "condition not met within \(limit)", file: file, line: line)
    }
}
