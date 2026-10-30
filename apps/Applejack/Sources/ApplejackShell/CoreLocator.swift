import Foundation

/// Where the core binary is: `APPLEJACK_CORE_BIN` for development, else
/// `applejack-core` beside the shell executable (`Contents/MacOS/` in the
/// bundle `cargo xtask app-bundle` assembles).
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
            let url = URL(fileURLWithPath: path)
            return isExecutable(url) ? url : nil
        }
        guard let executableURL else { return nil }
        let sibling = executableURL.deletingLastPathComponent().appendingPathComponent(Self.bundledName)
        return isExecutable(sibling) ? sibling : nil
    }
}
