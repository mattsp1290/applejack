import Foundation
import Observation

/// `steam.library` with install state per title, and `app.install` as a job.
/// The rows are shown in the shell's order, not the core's: a library runs to
/// a thousand titles, so the installed ones come first and each group is by
/// name, and a query narrows them.
@Observable @MainActor
public final class LibraryViewModel {
    public private(set) var library: FlowState<SteamLibraryResult> = .idle {
        didSet {
            sortedApps = Self.ordered(library.value?.apps ?? [])
            // `.loading` lists nothing; the highlight waits for the rows.
            if library.value != nil {
                dropHiddenSelection()
            }
        }
    }
    public private(set) var installs: [UInt32: FlowState<JobSnapshot>] = [:]
    /// The search field's text. Whitespace alone is no query.
    public var query: String = "" {
        didSet { dropHiddenSelection() }
    }
    /// The highlighted row's appid; cleared when a query hides that title or
    /// a reload no longer lists it.
    public var selection: UInt32?
    /// Every title: `installed` and `update_available` first, then the rest,
    /// each group in Finder's order (without regard to case or diacritics,
    /// `Trials 2` before `Trials 10`) and by appid for two titles of one
    /// name. Sorted once per library, not on every read.
    public private(set) var sortedApps: [LibraryEntry] = []
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    /// `sortedApps` narrowed to the query: a substring of the shown name (the
    /// name, or `App <appid>` without one) without regard to case or
    /// diacritics, or a prefix of the appid's digits.
    public var filteredApps: [LibraryEntry] {
        let query = trimmedQuery
        guard !query.isEmpty else { return sortedApps }
        return sortedApps.filter { Self.matches($0, query: query) }
    }

    /// The count line shown while a query is active; `nil` without one.
    public var matchCount: String? {
        guard !trimmedQuery.isEmpty else { return nil }
        return "Showing \(filteredApps.count) of \(sortedApps.count) titles"
    }

    /// The query as matched: without surrounding whitespace.
    public var trimmedQuery: String {
        query.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func dropHiddenSelection() {
        if let selection, !filteredApps.contains(where: { $0.appid == selection }) {
            self.selection = nil
        }
    }

    public static func ordered(_ apps: [LibraryEntry]) -> [LibraryEntry] {
        apps.sorted { lhs, rhs in
            if lhs.installState.isOnDisk != rhs.installState.isOnDisk {
                return lhs.installState.isOnDisk
            }
            switch lhs.title.localizedStandardCompare(rhs.title) {
            case .orderedAscending: return true
            case .orderedDescending: return false
            case .orderedSame: return lhs.appid < rhs.appid
            }
        }
    }

    public static func matches(_ entry: LibraryEntry, query: String) -> Bool {
        entry.title.localizedStandardRange(of: query) != nil
            || String(entry.appid).hasPrefix(query)
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

extension InstallState {
    /// Installed titles and those with an update waiting both have files on
    /// disk; the library lists them ahead of the rest.
    var isOnDisk: Bool {
        switch self {
        case .installed, .updateAvailable: true
        case .notInstalled: false
        }
    }
}
