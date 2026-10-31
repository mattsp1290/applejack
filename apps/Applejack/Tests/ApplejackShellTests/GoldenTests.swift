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
            ["empty.result.json", "hello.result.json", "host.doctor.result.json"]
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
        try check("config.get", ConfigGetParams.self)
        try check("config.set", ConfigSetParams.self)
        try check("config.import", ConfigImportParams.self)
        try check("engine.install", EngineInstallParams.self)
        try check("engine.use", EngineUseParams.self)
        try check("engine.d3dmetal.import", EngineD3dmetalImportParams.self)
        try check("steam.login.start", SteamLoginStartParams.self)
        try check("steam.login.complete", SteamLoginCompleteParams.self)
        try check("session.list", SessionListParams.self)
        for method in ["engine.list", "engine.doctor", "engine.probe_cpuid", "steam.logout", "host.doctor"] {
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
