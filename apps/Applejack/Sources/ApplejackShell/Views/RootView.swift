import SwiftUI

/// Every view model the window uses, built once per app.
@MainActor
public final class ShellModels {
    public let app: AppModel
    public let host: HostViewModel
    public let library: LibraryViewModel
    public let engines: EnginesViewModel
    public let steam: SteamAccountViewModel
    public let logs: LogsViewModel
    public let controllers: ControllersViewModel
    public let d3dmetal: D3DMetalImportViewModel

    public init(app: AppModel) {
        self.app = app
        host = HostViewModel(app: app)
        library = LibraryViewModel(app: app)
        engines = EnginesViewModel(app: app)
        steam = SteamAccountViewModel(app: app)
        logs = LogsViewModel(app: app)
        controllers = ControllersViewModel()
        d3dmetal = D3DMetalImportViewModel(app: app)
    }
}

public enum ShellSection: String, CaseIterable, Identifiable, Sendable {
    case library, engines, steam, logs, controllers, d3dmetal, host

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .library: "Library"
        case .engines: "Engines"
        case .steam: "Steam Account"
        case .logs: "Logs"
        case .controllers: "Controllers"
        case .d3dmetal: "D3DMetal"
        case .host: "Host"
        }
    }

    public var systemImage: String {
        switch self {
        case .library: "books.vertical"
        case .engines: "gearshape.2"
        case .steam: "person.crop.circle"
        case .logs: "doc.text"
        case .controllers: "gamecontroller"
        case .d3dmetal: "cube.transparent"
        case .host: "desktopcomputer"
        }
    }
}

public struct RootView: View {
    let models: ShellModels
    @State private var section: ShellSection?

    /// `initialSection` picks the sidebar row shown first; `main.swift` reads
    /// it from `APPLEJACK_SECTION` so a launch can land on any view.
    public init(models: ShellModels, initialSection: ShellSection? = nil) {
        self.models = models
        _section = State(initialValue: initialSection ?? .library)
    }

    public var body: some View {
        NavigationSplitView {
            List(ShellSection.allCases, selection: $section) { section in
                Label(section.title, systemImage: section.systemImage).tag(section)
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 200)
        } detail: {
            VStack(spacing: 0) {
                CoreBanner(app: models.app)
                detail
            }
        }
        .task { await models.app.start() }
    }

    @ViewBuilder
    private var detail: some View {
        switch section ?? .library {
        case .library: LibraryView(model: models.library, app: models.app)
        case .engines: EnginesView(model: models.engines)
        case .steam: SteamAccountView(model: models.steam)
        case .logs: LogsView(model: models.logs)
        case .controllers: ControllersView(model: models.controllers)
        case .d3dmetal: D3DMetalImportView(model: models.d3dmetal)
        case .host: HostView(model: models.host)
        }
    }
}
