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
        XCTAssertNil(library.installs[440], "a finished install hands the row back to the library's install state")
        XCTAssertTrue(try requests(for: "app.install")[0].contains("\"params\":{\"appid\":440,\"update\":false,\"verify\":false}"))
        XCTAssertEqual(try requests(for: "steam.library").count, 2, "a finished install reloads the library")
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
        XCTAssertTrue(imports[2].contains("\"accept_launch_overrides\":true,\"appid\":220,\"path\":\"/tmp/B.preset.json\""), imports[2])
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
        XCTAssertEqual(detail.runState.value, AppRunResult(id: .string("fake-session"), logPath: log.path))
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

    func testASecondRunWhileOneIsInFlightSendsNothing() async throws {
        let app = makeApp(environment: ["FAKE_CORE_RUN_DELAY_MS": "300"])
        let detail = GameDetailViewModel(app: app, appid: 220)
        let first = Task { await detail.run() }
        try await waitUntil { detail.runState == .loading }
        await detail.run()
        XCTAssertEqual(detail.runState, .loading, "the second Run returns at once and leaves the first in flight")
        await first.value
        XCTAssertNotNil(detail.runState.value)
        XCTAssertEqual(try requests(for: "app.run").count, 1)

        // Once the first returned, Run works again.
        await detail.run()
        XCTAssertEqual(try requests(for: "app.run").count, 2)
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
        XCTAssertTrue(imports[0].contains("\"params\":{\"license_acknowledged\":true,\"path\":\"\(volume.path)\"}"), imports[0])
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
            case ("malformedResponse", .malformedResponse), ("rpc", .rpc): break
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
