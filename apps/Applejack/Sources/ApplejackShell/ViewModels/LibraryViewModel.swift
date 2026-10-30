import Foundation
import Observation

/// `steam.library` with install state per title, and `app.install` as a job.
@Observable @MainActor
public final class LibraryViewModel {
    public private(set) var library: FlowState<SteamLibraryResult> = .idle
    public private(set) var installs: [UInt32: FlowState<JobSnapshot>] = [:]
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public func load(refresh: Bool = false) async {
        library = .loading
        if let next = await app.run({ try await $0.call("steam.library", params: SteamLibraryParams(refresh: refresh)) as SteamLibraryResult }) {
            library = next
        }
    }

    public func install(_ appid: UInt32) async {
        await app.runJob("app.install", params: AppInstallParams(appid: appid)) { installs[appid] = $0 }
    }
}
