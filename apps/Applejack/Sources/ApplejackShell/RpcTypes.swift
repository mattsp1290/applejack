import Foundation

// Mirrors of `applejack_core::rpc::wire`, `methods`, and `jobs`. The goldens
// under `crates/applejack-core/tests/rpc_golden/` pin both sides.

/// Any JSON value, for params and results the shell does not type.
public indirect enum JSONValue: Codable, Equatable, Sendable {
    case null
    case bool(Bool)
    case integer(Int64)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Int64.self) {
            self = .integer(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else if let value = try? container.decode([String: JSONValue].self) {
            self = .object(value)
        } else {
            throw DecodingError.dataCorruptedError(in: container, debugDescription: "not a JSON value")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case .bool(let value): try container.encode(value)
        case .integer(let value): try container.encode(value)
        case .number(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        case .array(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        }
    }
}

public enum RpcId: Codable, Hashable, Sendable {
    case number(Int64)
    case string(String)

    public init(from decoder: any Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let value = try? container.decode(Int64.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else {
            throw DecodingError.dataCorruptedError(in: container, debugDescription: "id must be a number or a string")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .number(let value): try container.encode(value)
        case .string(let value): try container.encode(value)
        }
    }
}

public enum RpcVersion {
    public static let text = "2.0"
}

public struct RpcRequest: Codable, Equatable, Sendable {
    public var jsonrpc: String
    public var id: RpcId
    public var method: String
    public var params: JSONValue

    public init(id: RpcId, method: String, params: JSONValue) {
        self.jsonrpc = RpcVersion.text
        self.id = id
        self.method = method
        self.params = params
    }
}

/// A response whose `result` is decoded as `Result`; `error` is set instead
/// when the call failed.
public struct RpcTypedResponse<Result: Decodable & Sendable>: Decodable, Sendable {
    public var jsonrpc: String
    public var id: RpcId?
    public var result: Result?
    public var error: RpcError?

    enum CodingKeys: String, CodingKey {
        case jsonrpc, id, result, error
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        jsonrpc = try container.decode(String.self, forKey: .jsonrpc)
        id = try container.decodeIfPresent(RpcId.self, forKey: .id)
        result = try container.decodeIfPresent(Result.self, forKey: .result)
        error = try container.decodeIfPresent(RpcError.self, forKey: .error)
    }
}

public typealias RpcResponse = RpcTypedResponse<JSONValue>

public struct RpcNotification: Codable, Equatable, Sendable {
    public var jsonrpc: String
    public var method: String
    public var params: JSONValue
}

public struct RpcError: Codable, Equatable, Sendable, Error {
    public var code: Int
    public var message: String
    public var data: JSONValue?

    public init(code: Int, message: String, data: JSONValue? = nil) {
        self.code = code
        self.message = message
        self.data = data
    }

    public var errorCode: RpcErrorCode? { RpcErrorCode(rawValue: code) }
}

/// `ErrorCode` in `rpc/wire.rs`; the raw values are the wire integers.
public enum RpcErrorCode: Int, Codable, Sendable, CaseIterable {
    case parseError = -32700
    case invalidRequest = -32600
    case methodNotFound = -32601
    case invalidParams = -32602
    case internalError = -32603
    case protocolMismatch = 1000
    case helloRequired = 1001
    case notImplemented = 1002
    case `internal` = 1003
    case jobFailed = 1004
}

public struct HelloParams: Codable, Equatable, Sendable {
    public var protocolVersion: UInt32

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
    }

    public init(protocolVersion: UInt32) {
        self.protocolVersion = protocolVersion
    }
}

public struct HelloResult: Codable, Equatable, Sendable {
    public var protocolVersion: UInt32
    public var coreVersion: String

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case coreVersion = "core_version"
    }
}

/// Params for methods that take none; encodes as `{}`.
public struct EmptyParams: Codable, Equatable, Sendable {
    public init() {}
}

public enum ScreenRecordingState: String, Codable, Sendable {
    case unknown
}

/// `applejack_core::host::HostFacts`.
public struct HostFacts: Codable, Equatable, Sendable {
    public var macosVersion: String
    public var arch: String?
    public var rosettaPresent: Bool?
    public var freeDiskBytes: UInt64?
    public var screenRecording: ScreenRecordingState
    public var developerIdIdentities: Int?

    enum CodingKeys: String, CodingKey {
        case macosVersion = "macos_version"
        case arch
        case rosettaPresent = "rosetta_present"
        case freeDiskBytes = "free_disk_bytes"
        case screenRecording = "screen_recording"
        case developerIdIdentities = "developer_id_identities"
    }
}

public struct JobId: Codable, Hashable, Sendable {
    public var rawValue: UInt64

    public init(rawValue: UInt64) { self.rawValue = rawValue }

    public init(from decoder: any Decoder) throws {
        rawValue = try decoder.singleValueContainer().decode(UInt64.self)
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

public struct JobStarted: Codable, Equatable, Sendable {
    public var jobId: JobId

    enum CodingKeys: String, CodingKey {
        case jobId = "job_id"
    }
}

public struct JobProgress: Codable, Equatable, Sendable {
    public static let method = "job.progress"

    public var jobId: JobId
    public var phase: String
    public var doneBytes: UInt64?
    public var totalBytes: UInt64?
    public var message: String

    enum CodingKeys: String, CodingKey {
        case jobId = "job_id"
        case phase
        case doneBytes = "done_bytes"
        case totalBytes = "total_bytes"
        case message
    }
}

public struct JobDone: Codable, Equatable, Sendable {
    public static let method = "job.done"

    public var jobId: JobId

    enum CodingKeys: String, CodingKey {
        case jobId = "job_id"
    }
}

public struct JobFailed: Codable, Equatable, Sendable {
    public static let method = "job.failed"

    public var jobId: JobId
    public var code: Int
    public var message: String

    enum CodingKeys: String, CodingKey {
        case jobId = "job_id"
        case code
        case message
    }
}

enum RpcCoding {
    /// Sorted keys match `serde_json`'s object order, so a request encodes
    /// byte-for-byte like its golden.
    static func encoder() -> JSONEncoder {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return encoder
    }

    static func decoder() -> JSONDecoder {
        JSONDecoder()
    }
}
