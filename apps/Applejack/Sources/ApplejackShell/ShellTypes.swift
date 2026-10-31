import Foundation

// Params mirrors for the methods the views call. `GoldenTests` pins each one
// to its request golden, and the engine and config results to their result
// goldens. The remaining result mirrors are provisional: the core has no
// result type for a pending method yet, so these follow the Rust structs the
// methods will serialize (`LoginStep`, `AppRow`, the `sessions` table) and
// gain goldens when the methods land.

public struct SteamLibraryParams: Codable, Equatable, Sendable {
    public var refresh: Bool

    public init(refresh: Bool) { self.refresh = refresh }
}

public struct AppInstallParams: Codable, Equatable, Sendable {
    public var appid: UInt32
    public var branch: String?
    public var update: Bool
    public var verify: Bool

    public init(appid: UInt32, branch: String? = nil, update: Bool = false, verify: Bool = false) {
        self.appid = appid
        self.branch = branch
        self.update = update
        self.verify = verify
    }
}

public struct AppRunParams: Codable, Equatable, Sendable {
    public var appid: UInt32
    public var probeDirect: Bool

    enum CodingKeys: String, CodingKey {
        case appid
        case probeDirect = "probe_direct"
    }

    public init(appid: UInt32, probeDirect: Bool = false) {
        self.appid = appid
        self.probeDirect = probeDirect
    }
}

/// The shell's proposal for the result of `app.run` until work package 5
/// defines it: the session's `id` and `log_path`, named after the fields of
/// `Session` in `applejack-engine` (which has no serde form yet). Both
/// members are optional so the shell decodes whatever the method returns
/// first and lights up the log tail once `log_path` is there.
public struct AppRunResult: Codable, Equatable, Sendable {
    public var id: JSONValue?
    public var logPath: String?

    enum CodingKeys: String, CodingKey {
        case id
        case logPath = "log_path"
    }

    public init(id: JSONValue? = nil, logPath: String? = nil) {
        self.id = id
        self.logPath = logPath
    }
}

public struct ConfigGetParams: Codable, Equatable, Sendable {
    public var appid: UInt32

    public init(appid: UInt32) { self.appid = appid }
}

public struct ConfigSetParams: Codable, Equatable, Sendable {
    public var appid: UInt32
    public var key: String
    public var value: JSONValue

    public init(appid: UInt32, key: String, value: JSONValue) {
        self.appid = appid
        self.key = key
        self.value = value
    }
}

public struct ConfigImportParams: Codable, Equatable, Sendable {
    public var appid: UInt32
    public var path: String
    public var acceptLaunchOverrides: Bool

    enum CodingKeys: String, CodingKey {
        case appid, path
        case acceptLaunchOverrides = "accept_launch_overrides"
    }

    public init(appid: UInt32, path: String, acceptLaunchOverrides: Bool) {
        self.appid = appid
        self.path = path
        self.acceptLaunchOverrides = acceptLaunchOverrides
    }
}

public struct EngineInstallParams: Codable, Equatable, Sendable {
    public var engineId: String

    enum CodingKeys: String, CodingKey {
        case engineId = "engine_id"
    }

    public init(engineId: String) { self.engineId = engineId }
}

public struct EngineUseParams: Codable, Equatable, Sendable {
    public var engineId: String
    public var version: String

    enum CodingKeys: String, CodingKey {
        case engineId = "engine_id"
        case version
    }

    public init(engineId: String, version: String) {
        self.engineId = engineId
        self.version = version
    }
}

public struct EngineD3dmetalImportParams: Codable, Equatable, Sendable {
    public var path: String
    /// The core refuses without it; the shell sets it only after the license
    /// dialog was accepted.
    public var licenseAcknowledged: Bool

    enum CodingKeys: String, CodingKey {
        case path
        case licenseAcknowledged = "license_acknowledged"
    }

    public init(path: String, licenseAcknowledged: Bool) {
        self.path = path
        self.licenseAcknowledged = licenseAcknowledged
    }
}

/// `LoginMode` in `methods.rs`: `{"kind":"qr"}` or
/// `{"kind":"credentials","account_name":…,"password":…}`.
public enum LoginMode: Codable, Equatable, Sendable {
    case qr
    case credentials(accountName: String, password: String)

    enum CodingKeys: String, CodingKey {
        case kind
        case accountName = "account_name"
        case password
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .kind) {
        case "qr":
            self = .qr
        case "credentials":
            self = .credentials(
                accountName: try container.decode(String.self, forKey: .accountName),
                password: try container.decode(String.self, forKey: .password)
            )
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: container, debugDescription: "unknown login mode \(other)")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .qr:
            try container.encode("qr", forKey: .kind)
        case .credentials(let accountName, let password):
            try container.encode("credentials", forKey: .kind)
            try container.encode(accountName, forKey: .accountName)
            try container.encode(password, forKey: .password)
        }
    }
}

public struct SteamLoginStartParams: Codable, Equatable, Sendable {
    public var mode: LoginMode

    public init(mode: LoginMode) { self.mode = mode }
}

public struct SteamLoginCompleteParams: Codable, Equatable, Sendable {
    public var sessionId: String
    public var steamGuardCode: String?

    enum CodingKeys: String, CodingKey {
        case sessionId = "session_id"
        case steamGuardCode = "steam_guard_code"
    }

    public init(sessionId: String, steamGuardCode: String?) {
        self.sessionId = sessionId
        self.steamGuardCode = steamGuardCode
    }
}

public struct SessionListParams: Codable, Equatable, Sendable {
    public var appid: UInt32?

    public init(appid: UInt32?) { self.appid = appid }
}

// MARK: - Provisional results

/// `Drm` in `applejack-config`.
public enum Drm: String, Codable, Sendable {
    case unknown, none, steam
}

public enum InstallState: String, Codable, Sendable {
    case installed
    case notInstalled = "not_installed"
    case updateAvailable = "update_available"
}

public struct LibraryEntry: Codable, Equatable, Sendable, Identifiable {
    public var appid: UInt32
    public var name: String?
    public var drm: Drm
    public var installState: InstallState

    enum CodingKeys: String, CodingKey {
        case appid, name, drm
        case installState = "install_state"
    }

    public var id: UInt32 { appid }
    public var title: String { name ?? "App \(appid)" }
}

public struct SteamLibraryResult: Codable, Equatable, Sendable {
    public var apps: [LibraryEntry]
}

public struct EngineEntry: Codable, Equatable, Sendable, Identifiable {
    public var engineId: String
    public var version: String
    public var installed: Bool
    public var active: Bool

    enum CodingKeys: String, CodingKey {
        case engineId = "engine_id"
        case version, installed, active
    }

    public var id: String { "\(engineId)@\(version)" }
}

public struct EngineListResult: Codable, Equatable, Sendable {
    public var engines: [EngineEntry]
}

/// `Severity` in `applejack-engine`.
public enum Severity: String, Codable, Sendable {
    case info, warning, error
}

/// `Finding` in `applejack-engine`.
public struct Finding: Codable, Equatable, Sendable {
    public var severity: Severity
    public var message: String
}

/// `DoctorReport` in `applejack-engine`.
public struct DoctorReport: Codable, Equatable, Sendable {
    public var runnable: Bool
    public var findings: [Finding]
}

/// `LoginStep` in `applejack-steam`, tagged by `kind`.
public enum LoginStep: Codable, Equatable, Sendable {
    case qrReady(payload: String)
    case guardRequired(factor: String)
    case done(accountName: String)

    enum CodingKeys: String, CodingKey {
        case kind, payload, factor
        case accountName = "account_name"
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(String.self, forKey: .kind) {
        case "qr_ready":
            self = .qrReady(payload: try container.decode(String.self, forKey: .payload))
        case "guard_required":
            self = .guardRequired(factor: try container.decode(String.self, forKey: .factor))
        case "done":
            self = .done(accountName: try container.decode(String.self, forKey: .accountName))
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: container, debugDescription: "unknown login step \(other)")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .qrReady(let payload):
            try container.encode("qr_ready", forKey: .kind)
            try container.encode(payload, forKey: .payload)
        case .guardRequired(let factor):
            try container.encode("guard_required", forKey: .kind)
            try container.encode(factor, forKey: .factor)
        case .done(let accountName):
            try container.encode("done", forKey: .kind)
            try container.encode(accountName, forKey: .accountName)
        }
    }
}

public struct LoginSession: Codable, Equatable, Sendable {
    public var sessionId: String
    public var step: LoginStep

    enum CodingKeys: String, CodingKey {
        case sessionId = "session_id"
        case step
    }
}

/// One `sessions` row.
public struct SessionEntry: Codable, Equatable, Sendable, Identifiable {
    public var id: Int64
    public var appid: UInt32
    public var startedAt: String
    public var endedAt: String?
    public var engineId: String?
    public var engineVersion: String?
    public var exitCode: Int32?
    public var logPath: String?

    enum CodingKeys: String, CodingKey {
        case id, appid
        case startedAt = "started_at"
        case endedAt = "ended_at"
        case engineId = "engine_id"
        case engineVersion = "engine_version"
        case exitCode = "exit_code"
        case logPath = "log_path"
    }
}

public struct SessionListResult: Codable, Equatable, Sendable {
    public var sessions: [SessionEntry]
}

/// `GraphicsBackend` in `applejack-config`.
public enum GraphicsBackend: String, Codable, Sendable, CaseIterable {
    case dxmt
    case wined3d
    case vkd3dProton = "vkd3d-proton"
    case d3dmetal
}

/// `SyncMode` in `applejack-config`.
public enum SyncMode: String, Codable, Sendable, CaseIterable {
    case msync, esync
    case disabled = "none"
}

/// `LaunchMode` in `applejack-config`.
public enum LaunchMode: String, Codable, Sendable, CaseIterable {
    case client, direct
}

/// `GameConfig` in `applejack-config`: one `game_config` row, every column
/// optional. `config.set` takes one `key` from `ConfigKey` and a JSON value.
public struct GameConfig: Codable, Equatable, Sendable {
    public var engineId: String?
    public var engineVersion: String?
    public var graphicsBackend: GraphicsBackend?
    public var syncMode: SyncMode?
    public var hidrawEnabled: Bool?
    public var steamInput: Bool?
    public var steamOverlay: Bool?
    public var rosettaAdvertiseAvx: Bool?
    public var extraEnv: [String: String]
    public var launchMode: LaunchMode?
    public var launchExeOverride: String?
    public var launchArgsOverride: String?
    public var directVerifiedAt: String?
    public var isolatedPrefix: Bool?
    public var dx12Expectation: String?
    public var notes: String?

    enum CodingKeys: String, CodingKey {
        case engineId = "engine_id"
        case engineVersion = "engine_version"
        case graphicsBackend = "graphics_backend"
        case syncMode = "sync_mode"
        case hidrawEnabled = "hidraw_enabled"
        case steamInput = "steam_input"
        case steamOverlay = "steam_overlay"
        case rosettaAdvertiseAvx = "rosetta_advertise_avx"
        case extraEnv = "extra_env"
        case launchMode = "launch_mode"
        case launchExeOverride = "launch_exe_override"
        case launchArgsOverride = "launch_args_override"
        case directVerifiedAt = "direct_verified_at"
        case isolatedPrefix = "isolated_prefix"
        case dx12Expectation = "dx12_expectation"
        case notes
    }

    public init(from decoder: any Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        engineId = try container.decodeIfPresent(String.self, forKey: .engineId)
        engineVersion = try container.decodeIfPresent(String.self, forKey: .engineVersion)
        graphicsBackend = try container.decodeIfPresent(GraphicsBackend.self, forKey: .graphicsBackend)
        syncMode = try container.decodeIfPresent(SyncMode.self, forKey: .syncMode)
        hidrawEnabled = try container.decodeIfPresent(Bool.self, forKey: .hidrawEnabled)
        steamInput = try container.decodeIfPresent(Bool.self, forKey: .steamInput)
        steamOverlay = try container.decodeIfPresent(Bool.self, forKey: .steamOverlay)
        rosettaAdvertiseAvx = try container.decodeIfPresent(Bool.self, forKey: .rosettaAdvertiseAvx)
        extraEnv = try container.decodeIfPresent([String: String].self, forKey: .extraEnv) ?? [:]
        launchMode = try container.decodeIfPresent(LaunchMode.self, forKey: .launchMode)
        launchExeOverride = try container.decodeIfPresent(String.self, forKey: .launchExeOverride)
        launchArgsOverride = try container.decodeIfPresent(String.self, forKey: .launchArgsOverride)
        directVerifiedAt = try container.decodeIfPresent(String.self, forKey: .directVerifiedAt)
        isolatedPrefix = try container.decodeIfPresent(Bool.self, forKey: .isolatedPrefix)
        dx12Expectation = try container.decodeIfPresent(String.self, forKey: .dx12Expectation)
        notes = try container.decodeIfPresent(String.self, forKey: .notes)
    }

    public init() {
        extraEnv = [:]
    }
}

/// The `config.set` keys, one per `game_config` column the editor shows.
public enum ConfigKey: String, CaseIterable, Sendable {
    case engineId = "engine_id"
    case engineVersion = "engine_version"
    case graphicsBackend = "graphics_backend"
    case syncMode = "sync_mode"
    case hidrawEnabled = "hidraw_enabled"
    case steamInput = "steam_input"
    case steamOverlay = "steam_overlay"
    case rosettaAdvertiseAvx = "rosetta_advertise_avx"
    case launchMode = "launch_mode"
    case launchExeOverride = "launch_exe_override"
    case launchArgsOverride = "launch_args_override"
    case isolatedPrefix = "isolated_prefix"
    case dx12Expectation = "dx12_expectation"
    case notes

    public enum Kind: Equatable, Sendable {
        case text
        case toggle
        case choice([String])
    }

    public var kind: Kind {
        switch self {
        case .graphicsBackend: .choice(GraphicsBackend.allCases.map(\.rawValue))
        case .syncMode: .choice(SyncMode.allCases.map(\.rawValue))
        case .launchMode: .choice(LaunchMode.allCases.map(\.rawValue))
        case .hidrawEnabled, .steamInput, .steamOverlay, .rosettaAdvertiseAvx, .isolatedPrefix: .toggle
        case .engineId, .engineVersion, .launchExeOverride, .launchArgsOverride, .dx12Expectation, .notes: .text
        }
    }
}

extension GameConfig {
    /// The stored value of `key` as JSON; `.null` when unset.
    public func value(_ key: ConfigKey) -> JSONValue {
        func text(_ value: String?) -> JSONValue { value.map(JSONValue.string) ?? .null }
        func flag(_ value: Bool?) -> JSONValue { value.map(JSONValue.bool) ?? .null }
        return switch key {
        case .engineId: text(engineId)
        case .engineVersion: text(engineVersion)
        case .graphicsBackend: text(graphicsBackend?.rawValue)
        case .syncMode: text(syncMode?.rawValue)
        case .hidrawEnabled: flag(hidrawEnabled)
        case .steamInput: flag(steamInput)
        case .steamOverlay: flag(steamOverlay)
        case .rosettaAdvertiseAvx: flag(rosettaAdvertiseAvx)
        case .launchMode: text(launchMode?.rawValue)
        case .launchExeOverride: text(launchExeOverride)
        case .launchArgsOverride: text(launchArgsOverride)
        case .isolatedPrefix: flag(isolatedPrefix)
        case .dx12Expectation: text(dx12Expectation)
        case .notes: text(notes)
        }
    }
}

/// One item of an `ImportPlan` (`Accepted` or `Confirmation`).
public struct ImportItem: Codable, Equatable, Sendable, Identifiable {
    public var field: String
    public var value: String

    public var id: String { field }
}

/// `Rejected` in `applejack-config`.
public struct RejectedItem: Codable, Equatable, Sendable, Identifiable {
    public var field: String
    public var value: String
    public var reason: String

    public var id: String { field }
}

/// The outcome of `config.import`: the plan, and whether it was applied.
/// `applied` is false while `confirmations` is non-empty and the request
/// did not accept launch overrides.
public struct ConfigImportResult: Codable, Equatable, Sendable {
    public var applied: Bool
    public var presetAppid: UInt32?
    public var accepted: [ImportItem]
    public var rejected: [RejectedItem]
    public var confirmations: [ImportItem]

    enum CodingKeys: String, CodingKey {
        case applied
        case presetAppid = "preset_appid"
        case accepted, rejected, confirmations
    }

    public var needsConfirmation: Bool { !applied && !confirmations.isEmpty }
}

extension JSONValue {
    public var stringValue: String? {
        if case .string(let value) = self { return value }
        return nil
    }

    public var boolValue: Bool? {
        if case .bool(let value) = self { return value }
        return nil
    }

    /// A one-line rendering for lists and labels.
    public var display: String {
        switch self {
        case .null: "unset"
        case .bool(let value): value ? "true" : "false"
        case .integer(let value): String(value)
        case .number(let value): String(value)
        case .string(let value): value
        case .array(let values): "[" + values.map(\.display).joined(separator: ", ") + "]"
        case .object(let values):
            "{" + values.keys.sorted().map { "\($0): \(values[$0]!.display)" }.joined(separator: ", ") + "}"
        }
    }
}
