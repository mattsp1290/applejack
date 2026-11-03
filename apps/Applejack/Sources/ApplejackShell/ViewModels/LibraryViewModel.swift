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

    /// A refresh is a job, since reading a library can outlast a request; the
    /// rows come from the cache once it ends.
    public func load(refresh: Bool = false) async {
        library = .loading
        if refresh {
            var failure: FlowState<SteamLibraryResult>?
            await app.runJob("steam.library", params: SteamLibraryParams(refresh: true)) { state in
                switch state {
                case .failed, .unavailable: failure = state.map { _ in SteamLibraryResult(apps: [], fetchedAt: nil, jobId: nil) }
                default: break
                }
            }
            if let failure {
                library = failure
                return
            }
        }
        if let next = await app.run({ try await $0.call("steam.library", params: SteamLibraryParams(refresh: false)) as SteamLibraryResult }) {
            library = next
        }
    }

    /// `app.install` as a job; a finished one gives the row back to the
    /// library's install state, reloaded.
    public func install(_ appid: UInt32) async {
        await app.runJob("app.install", params: AppInstallParams(appid: appid)) { installs[appid] = $0 }
        if case .ready(let snapshot) = installs[appid], snapshot.finished {
            installs[appid] = nil
            await load()
        }
    }
}
