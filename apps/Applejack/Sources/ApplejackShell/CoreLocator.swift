import Foundation

/// Where the core binary is: `APPLEJACK_CORE_BIN` for development, else
/// `applejack-core` beside the shell executable (`Contents/MacOS/` in the
/// bundle `cargo xtask app-bundle` assembles). A set `APPLEJACK_CORE_BIN` is
/// used as given, executable or not: a bad path then fails the spawn, which
/// the banner reports with the path and a Restart, instead of reading as
/// "not set".
public struct CoreLocator: Sendable {
    public static let environmentVariable = "APPLEJACK_CORE_BIN"
    public static let bundledName = "applejack-core"

    public var environment: [String: String]
    public var executableURL: URL?
    public var isExecutable: @Sendable (URL) -> Bool

    public init(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        executableURL: URL? = Bundle.main.executableURL,
        isExecutable: @escaping @Sendable (URL) -> Bool = { FileManager.default.isExecutableFile(atPath: $0.path) }
    ) {
        self.environment = environment
        self.executableURL = executableURL
        self.isExecutable = isExecutable
    }

    public func locate() -> URL? {
        if let path = environment[Self.environmentVariable], !path.isEmpty {
            return URL(fileURLWithPath: path)
        }
        guard let executableURL else { return nil }
        let sibling = executableURL.deletingLastPathComponent().appendingPathComponent(Self.bundledName)
        return isExecutable(sibling) ? sibling : nil
    }
}
