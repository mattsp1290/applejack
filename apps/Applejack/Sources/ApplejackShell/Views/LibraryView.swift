import SwiftUI

public struct LibraryView: View {
    let models: ShellModels
    var model: LibraryViewModel { models.library }

    public init(models: ShellModels) {
        self.models = models
    }

    public var body: some View {
        NavigationStack {
            FlowStateView(model.library, retry: { Task { await model.load() } }) { library in
                List(library.apps) { entry in
                    HStack {
                        NavigationLink(value: entry.appid) {
                            VStack(alignment: .leading) {
                                Text(entry.title)
                                Text("App \(entry.appid) · \(entry.drm.rawValue) DRM").font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        installColumn(entry)
                    }
                }
            }
            .navigationDestination(for: UInt32.self) { appid in
                GameDetailView(models: models, appid: appid)
            }
            .navigationTitle("Library")
            .toolbar {
                Button("Refresh") { Task { await model.load(refresh: true) } }
            }
        }
        .task { await model.load() }
    }

    @ViewBuilder
    private func installColumn(_ entry: LibraryEntry) -> some View {
        if let install = model.installs[entry.appid], install != .idle {
            JobProgressView(install, title: "Install").frame(maxWidth: 260)
        } else {
            switch entry.installState {
            case .installed:
                Label("Installed", systemImage: "checkmark").foregroundStyle(.secondary)
            case .notInstalled:
                Button("Install") { Task { await model.install(entry.appid) } }
            case .updateAvailable:
                Button("Update") { Task { await model.install(entry.appid) } }
            }
        }
    }
}
