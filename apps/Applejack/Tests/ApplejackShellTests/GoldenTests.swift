import Foundation
import XCTest

@testable import ApplejackShell

/// Every golden under `crates/applejack-core/tests/rpc_golden/` decodes into
/// the Swift mirrors.
final class GoldenTests: XCTestCase {
    static let goldenDirectory = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // Tests/ApplejackShellTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // apps/Applejack
        .deletingLastPathComponent()  // apps
        .deletingLastPathComponent()  // repository root
        .appendingPathComponent("crates/applejack-core/tests/rpc_golden")

    func goldens(suffix: String) throws -> [URL] {
        let files = try FileManager.default.contentsOfDirectory(
            at: Self.goldenDirectory, includingPropertiesForKeys: nil
        )
        return files.filter { $0.lastPathComponent.hasSuffix(suffix) }.sorted { $0.path < $1.path }
    }

    func testEveryRequestGoldenDecodes() throws {
        let requests = try goldens(suffix: ".request.json")
        XCTAssertGreaterThanOrEqual(requests.count, 28, "expected one request per table method")
        for file in requests {
            let request = try RpcCoding.decoder().decode(RpcRequest.self, from: Data(contentsOf: file))
            XCTAssertEqual(request.jsonrpc, RpcVersion.text, file.lastPathComponent)
            let expectedName = file.lastPathComponent.replacingOccurrences(of: ".request.json", with: "")
            XCTAssertEqual(request.method, expectedName, file.lastPathComponent)
            // The request re-encodes to the same bytes the Rust types produced.
            var reencoded = try RpcCoding.encoder().encode(request)
            reencoded.append(UInt8(ascii: "\n"))
            let compact = try JSONSerialization.data(
                withJSONObject: JSONSerialization.jsonObject(with: Data(contentsOf: file)),
                options: [.sortedKeys, .withoutEscapingSlashes]
            )
            XCTAssertEqual(reencoded.dropLast(), compact, file.lastPathComponent)
        }
    }

    func testEveryResultGoldenDecodes() throws {
        let results = try goldens(suffix: ".result.json")
        XCTAssertEqual(
            results.map(\.lastPathComponent),
            [
                "app.cancel.result.json", "app.install.result.json", "app.run.result.json",
                "config.export.result.json", "config.get.result.json", "config.import.result.json",
                "empty.result.json", "engine.d3dmetal.import.result.json", "engine.doctor.result.json",
                "engine.install.result.json", "engine.list.result.json", "engine.probe_cpuid.result.json",
                "hello.result.json", "host.doctor.result.json", "prefix.snapshot.result.json",
                "runtime.steam.install.result.json", "runtime.steam.status.result.json",
                "session.list.result.json", "steam.library.result.json",
                "steam.login.complete.result.json", "steam.login.start.result.json",
            ]
        )
        for file in results {
            let response = try RpcCoding.decoder().decode(RpcResponse.self, from: Data(contentsOf: file))
            XCTAssertNotNil(response.result, file.lastPathComponent)
            XCTAssertNil(response.error, file.lastPathComponent)
        }
    }

    /// Decodes a result golden as `Result` and checks that the Swift mirror
    /// re-encodes to the same bytes, so a field added on the Rust side fails
    /// here rather than going unnoticed until a view needs it.
    func typedResult<Result: Codable & Sendable>(_ file: String, as _: Result.Type) throws -> Result {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent(file))
        let response = try RpcCoding.decoder().decode(RpcTypedResponse<Result>.self, from: data)
        let result = try XCTUnwrap(response.result, file)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any], file)
        let compact = try JSONSerialization.data(
            withJSONObject: try XCTUnwrap(object["result"], file),
            options: [.sortedKeys, .withoutEscapingSlashes]
        )
        XCTAssertEqual(try RpcCoding.encoder().encode(result), compact, "\(file): the Swift mirror drops or adds a field")
        return result
    }

    func testHelloResultGoldenIsTyped() throws {
        let hello = try typedResult("hello.result.json", as: HelloResult.self)
        XCTAssertEqual(hello.protocolVersion, CoreClient.protocolVersion)
    }

    func testHostDoctorResultGoldenIsTyped() throws {
        let facts = try typedResult("host.doctor.result.json", as: HostFacts.self)
        XCTAssertEqual(facts.macosVersion, "26.0.1")
        XCTAssertEqual(facts.freeDiskBytes, 1_168_608_788_480)
        XCTAssertEqual(facts.screenRecording, .unknown)
    }

    func testEmptyResultGoldenIsTyped() throws {
        let empty = try typedResult("empty.result.json", as: EmptyResult.self)
        XCTAssertEqual(empty, EmptyResult())
        let untyped = try typedResult("empty.result.json", as: JSONValue.self)
        XCTAssertEqual(untyped, .object([:]))
    }

    func testNullResultIsPresentNotAbsent() throws {
        let line = Data(#"{"jsonrpc":"2.0","id":1,"result":null}"#.utf8)
        let untyped = try RpcCoding.decoder().decode(RpcResponse.self, from: line)
        XCTAssertEqual(untyped.result, .null)
        XCTAssertThrowsError(try RpcCoding.decoder().decode(RpcTypedResponse<HelloResult>.self, from: line))
    }

    func testResponseWithAnotherJsonrpcVersionIsRejected() {
        let line = Data(#"{"jsonrpc":"1.0","id":1,"result":{}}"#.utf8)
        XCTAssertThrowsError(try RpcCoding.decoder().decode(RpcResponse.self, from: line))
    }

    private func decodeNotification<Params: Decodable>(_ method: String, as _: Params.Type) throws -> Params {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("\(method).notification.json"))
        let notification = try RpcCoding.decoder().decode(RpcNotification.self, from: data)
        XCTAssertEqual(notification.method, method)
        let params = try RpcCoding.encoder().encode(notification.params)
        return try RpcCoding.decoder().decode(Params.self, from: params)
    }

    /// The mirrors the views decode read the core's real result shapes.
    func testResultMirrorsDecodeTheResultGoldens() throws {
        let engines = try typedResult("engine.list.result.json", as: EngineListResult.self)
        XCTAssertEqual(engines.engines.first?.id, "rosetta-wine@11.0-1")
        XCTAssertEqual(engines.engines.first?.active, true)
        _ = try typedResult("engine.install.result.json", as: JobStarted.self)
        let doctor = try typedResult("engine.doctor.result.json", as: DoctorReport.self)
        XCTAssertEqual(doctor.findings.first?.severity, .error)
        let configData = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("config.get.result.json"))
        let config = try XCTUnwrap(RpcCoding.decoder().decode(RpcTypedResponse<GameConfig>.self, from: configData).result)
        XCTAssertEqual(config.graphicsBackend, .dxmt)
        XCTAssertEqual(config.syncMode, .disabled)
        XCTAssertEqual(config.hidrawEnabled, true)
        let run = try typedResult("app.run.result.json", as: AppRunResult.self)
        XCTAssertEqual(run.id, 12)
        XCTAssertEqual(run.jobId, 7)
        XCTAssertEqual(run.logPath, "/data/logs/run-19a-1f-0/wine.log")
        _ = try typedResult("app.cancel.result.json", as: JobStarted.self)
        let sessions = try typedResult("session.list.result.json", as: SessionListResult.self)
        XCTAssertEqual(sessions.sessions.first?.state, .exited)
        XCTAssertEqual(sessions.sessions.first?.exitCode, 0)
        let login = try typedResult("steam.login.start.result.json", as: LoginSession.self)
        XCTAssertEqual(login.step, .guardRequired(factor: "device_code"))
        let done = try typedResult("steam.login.complete.result.json", as: LoginSession.self)
        XCTAssertEqual(done.step, .done(accountName: "fixture-user"))
        let steamLibrary = try typedResult("steam.library.result.json", as: SteamLibraryResult.self)
        XCTAssertEqual(steamLibrary.apps.first?.installState, .updateAvailable)
        XCTAssertEqual(steamLibrary.jobId, 8)
        _ = try typedResult("app.install.result.json", as: JobStarted.self)
        let plan = try typedResult("config.import.result.json", as: ConfigImportResult.self)
        XCTAssertTrue(plan.needsConfirmation)
        XCTAssertEqual(plan.rejected.first?.field, "extra_env.PATH")
        _ = try typedResult("runtime.steam.install.result.json", as: JobStarted.self)
        let runtime = try typedResult("runtime.steam.status.result.json", as: ClientRuntimeStatus.self)
        XCTAssertEqual(runtime.state, .needsLogin)
        XCTAssertEqual(runtime.buildId, "1761720601")
        XCTAssertTrue(runtime.running)
        let snapshot = try typedResult("prefix.snapshot.result.json", as: SnapshotResult.self)
        XCTAssertEqual(snapshot.prefix, "220")
        XCTAssertEqual(snapshot.timestamp, "20261031T120000Z")
        let export = try typedResult("config.export.result.json", as: ConfigExportResult.self)
        XCTAssertEqual(export.path, "/tmp/220.preset.json")
    }

    func testJobMirrorsDecodeTheNotificationGoldens() throws {
        let progress = try decodeNotification(JobProgress.method, as: JobProgress.self)
        XCTAssertEqual(progress.doneBytes, 1024)
        XCTAssertNil(progress.totalBytes)
        _ = try decodeNotification(JobDone.method, as: JobDone.self)
        let failed = try decodeNotification(JobFailed.method, as: JobFailed.self)
        XCTAssertEqual(RpcErrorCode(rawValue: failed.code), .checksumMismatch)
    }

    func testErrorCodesGoldenMatchesTheSwiftEnum() throws {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("error_codes.json"))
        let golden = try RpcCoding.decoder().decode([String: Int].self, from: data)
        XCTAssertEqual(golden.count, RpcErrorCode.allCases.count, "RpcErrorCode has a different number of cases than ErrorCode")
        XCTAssertEqual(Set(RpcErrorCode.allCases.map(\.rawValue)), Set(golden.values))
    }

    /// Every params mirror the views send re-encodes to its request golden's
    /// `params`, so a renamed Rust field fails here before a view sends it.
    func testParamsMirrorsMatchTheRequestGoldens() throws {
        func check<Params: Codable & Equatable>(_ method: String, _: Params.Type, file: StaticString = #filePath, line: UInt = #line) throws {
            let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("\(method).request.json"))
            let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any], method)
            let golden = try JSONSerialization.data(withJSONObject: try XCTUnwrap(object["params"], method), options: [.sortedKeys, .withoutEscapingSlashes])
            let decoded = try RpcCoding.decoder().decode(Params.self, from: golden)
            XCTAssertEqual(try RpcCoding.encoder().encode(decoded), golden, "\(method): the params mirror drops or adds a field", file: file, line: line)
        }
        try check("steam.library", SteamLibraryParams.self)
        try check("app.install", AppInstallParams.self)
        try check("app.run", AppRunParams.self)
        try check("app.cancel", AppCancelParams.self)
        try check("config.get", ConfigGetParams.self)
        try check("config.set", ConfigSetParams.self)
        try check("config.import", ConfigImportParams.self)
        try check("engine.install", EngineInstallParams.self)
        try check("engine.use", EngineUseParams.self)
        try check("engine.d3dmetal.import", EngineD3dmetalImportParams.self)
        try check("steam.login.start", SteamLoginStartParams.self)
        try check("steam.login.complete", SteamLoginCompleteParams.self)
        try check("session.list", SessionListParams.self)
        try check("runtime.steam.repair", RuntimeSteamRepairParams.self)
        try check("config.export", ConfigExportParams.self)
        try check("prefix.snapshot", PrefixSnapshotParams.self)
        try check("prefix.restore", PrefixRestoreParams.self)
        try check("prefix.reset", PrefixResetParams.self)
        for method in [
            "engine.list", "engine.doctor", "engine.probe_cpuid", "steam.logout", "host.doctor",
            "runtime.steam.install", "runtime.steam.update", "runtime.steam.stop", "runtime.steam.status",
        ] {
            try check(method, EmptyParams.self)
        }
        let credentials = try RpcCoding.encoder().encode(SteamLoginStartParams(mode: .credentials(accountName: "a", password: "p")))
        XCTAssertEqual(String(decoding: credentials, as: UTF8.self), #"{"mode":{"account_name":"a","kind":"credentials","password":"p"}}"#)
    }

    func testHelloRequestMatchesWhatTheClientSends() throws {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("hello.request.json"))
        let request = try RpcCoding.decoder().decode(RpcRequest.self, from: data)
        XCTAssertEqual(request.params, .object(["protocol_version": .integer(Int64(CoreClient.protocolVersion))]))
        // The bytes `CoreClient` writes, not just the public mirror.
        let outgoing = CoreClient.OutgoingRequest(
            id: .number(1), method: "hello", params: HelloParams(protocolVersion: CoreClient.protocolVersion)
        )
        var line = try CoreClient.encodeLine(outgoing)
        XCTAssertEqual(line.last, UInt8(ascii: "\n"))
        line.removeLast()
        let compact = try JSONSerialization.data(
            withJSONObject: JSONSerialization.jsonObject(with: data),
            options: [.sortedKeys, .withoutEscapingSlashes]
        )
        XCTAssertEqual(line, compact)
    }
}
