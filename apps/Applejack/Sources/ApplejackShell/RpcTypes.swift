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
/// when the call failed. Only `"jsonrpc": "2.0"` decodes, as on the Rust side.
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
        guard jsonrpc == RpcVersion.text else {
            throw DecodingError.dataCorruptedError(
                forKey: .jsonrpc, in: container,
                debugDescription: "jsonrpc must be \"\(RpcVersion.text)\", got \"\(jsonrpc)\""
            )
        }
        id = try container.decodeIfPresent(RpcId.self, forKey: .id)
        // A present `result` is decoded even when it is `null`: `JSONValue`
        // reads `.null`, a typed result fails to decode. The core never sends
        // `null` (a method with nothing to return answers `{}`), so neither
        // is mistaken for an absent result.
        if container.contains(.result) {
            result = try container.decode(Result.self, forKey: .result)
        } else {
            result = nil
        }
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
    case engineNotInstalled = 1005
    case checksumMismatch = 1006
    case invalidBundle = 1007
    case diskFull = 1008
    case downloadFailed = 1009
    case unsignedEngineRefused = 1010
    case prefixBusy = 1011
    case titleNotInstalled = 1012
    case downloadInProgress = 1013
    case alreadyRunning = 1014
    case clientRuntimeUnavailable = 1015
    case noLaunchExe = 1016
    case notRunning = 1017
    case titleExitedEarly = 1018
    case prefixUnusable = 1019
    case steamLoginRequired = 1020
    case steamLoginFailed = 1021
    case steamGuardRejected = 1022
    case steamRateLimited = 1023
    case steamUnavailable = 1024
    case appNotOwned = 1025
    case titleNotInstallable = 1026
    case launchExeAmbiguous = 1027
    case installCancelled = 1028
    case lockHeld = 1029
    case alreadyInstalled = 1030
    case steamClientOwnsTitle = 1031
    case verifyFailed = 1032
    case bootstrapperUnverified = 1033
    case clientRuntimeBroken = 1034
    case clientRedownloadRefused = 1035
    case clientLaunchTimedOut = 1036
    case titleUpdatePending = 1037
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

/// The result of a method with nothing to return (`EmptyResult` in
/// `methods.rs`). The core sends `{}`, never `null`.
public struct EmptyResult: Codable, Equatable, Sendable {
    public init() {}
}

public enum ScreenRecordingState: String, Codable, Sendable {
    case unknown
}

public enum ControllerTransport: String, Codable, Sendable {
    case usb
    case bluetooth
}

public enum ControllerKind: String, Codable, Sendable {
    case dualshock4
    case dualsense
    case xbox
    case other
}

/// `applejack_core::host::ControllerFact`: one pad `system_profiler` lists.
public struct ControllerFact: Codable, Equatable, Sendable {
    public var name: String
    public var transport: ControllerTransport
    public var vendorId: String
    public var productId: String
    public var connected: Bool
    public var kind: ControllerKind

    enum CodingKeys: String, CodingKey {
        case name
        case transport
        case vendorId = "vendor_id"
        case productId = "product_id"
        case connected
        case kind
    }
}

/// `applejack_core::host::HostFacts`.
public struct HostFacts: Codable, Equatable, Sendable {
    public var macosVersion: String
    public var arch: String?
    public var rosettaPresent: Bool?
    public var freeDiskBytes: UInt64?
    public var screenRecording: ScreenRecordingState
    public var developerIdIdentities: Int?
    public var controllers: [ControllerFact]

    enum CodingKeys: String, CodingKey {
        case macosVersion = "macos_version"
        case arch
        case rosettaPresent = "rosetta_present"
        case freeDiskBytes = "free_disk_bytes"
        case screenRecording = "screen_recording"
        case developerIdIdentities = "developer_id_identities"
        case controllers
    }

    public init(
        macosVersion: String,
        arch: String?,
        rosettaPresent: Bool?,
        freeDiskBytes: UInt64?,
        screenRecording: ScreenRecordingState,
        developerIdIdentities: Int?,
        controllers: [ControllerFact]
    ) {
        self.macosVersion = macosVersion
        self.arch = arch
        self.rosettaPresent = rosettaPresent
        self.freeDiskBytes = freeDiskBytes
        self.screenRecording = screenRecording
        self.developerIdIdentities = developerIdIdentities
        self.controllers = controllers
    }

    /// `controllers` reads as empty when absent, as the Rust side's
    /// `#[serde(default)]` does, so a core from before the inventory still
    /// decodes.
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        macosVersion = try container.decode(String.self, forKey: .macosVersion)
        arch = try container.decodeIfPresent(String.self, forKey: .arch)
        rosettaPresent = try container.decodeIfPresent(Bool.self, forKey: .rosettaPresent)
        freeDiskBytes = try container.decodeIfPresent(UInt64.self, forKey: .freeDiskBytes)
        screenRecording = try container.decode(ScreenRecordingState.self, forKey: .screenRecording)
        developerIdIdentities = try container.decodeIfPresent(Int.self, forKey: .developerIdIdentities)
        controllers = try container.decodeIfPresent([ControllerFact].self, forKey: .controllers) ?? []
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

/// `ClientRuntimeState` in `runtime/steam_client.rs`.
public enum ClientRuntimeState: String, Codable, Sendable {
    case absent
    case installing
    case needsLogin = "needs_login"
    case ready
    case updating
    case broken
}

/// The result of `runtime.steam.status` (`ClientRuntimeStatus`).
public struct ClientRuntimeStatus: Codable, Equatable, Sendable {
    public var state: ClientRuntimeState
    public var prefix: String
    public var buildId: String?
    public var running: Bool
    public var snapshots: Int
    public var detail: String

    enum CodingKeys: String, CodingKey {
        case state, prefix, running, snapshots, detail
        case buildId = "build_id"
    }

    public init(state: ClientRuntimeState, prefix: String, buildId: String?, running: Bool, snapshots: Int, detail: String) {
        self.state = state
        self.prefix = prefix
        self.buildId = buildId
        self.running = running
        self.snapshots = snapshots
        self.detail = detail
    }
}
