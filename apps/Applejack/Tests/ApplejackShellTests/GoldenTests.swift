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
        XCTAssertEqual(results.map(\.lastPathComponent), ["hello.result.json", "host.doctor.result.json"])
        for file in results {
            let response = try RpcCoding.decoder().decode(RpcResponse.self, from: Data(contentsOf: file))
            XCTAssertNotNil(response.result, file.lastPathComponent)
            XCTAssertNil(response.error, file.lastPathComponent)
        }
    }

    func testHelloResultGoldenIsTyped() throws {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("hello.result.json"))
        let response = try RpcCoding.decoder().decode(RpcTypedResponse<HelloResult>.self, from: data)
        XCTAssertEqual(response.result?.protocolVersion, CoreClient.protocolVersion)
    }

    func testHostDoctorResultGoldenIsTyped() throws {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("host.doctor.result.json"))
        let response = try RpcCoding.decoder().decode(RpcTypedResponse<HostFacts>.self, from: data)
        let facts = try XCTUnwrap(response.result)
        XCTAssertEqual(facts.macosVersion, "26.0.1")
        XCTAssertEqual(facts.freeDiskBytes, 1_168_608_788_480)
        XCTAssertEqual(facts.screenRecording, .unknown)
    }

    func testHelloRequestMatchesWhatTheClientSends() throws {
        let data = try Data(contentsOf: Self.goldenDirectory.appendingPathComponent("hello.request.json"))
        let request = try RpcCoding.decoder().decode(RpcRequest.self, from: data)
        XCTAssertEqual(request.params, .object(["protocol_version": .integer(Int64(CoreClient.protocolVersion))]))
    }
}
