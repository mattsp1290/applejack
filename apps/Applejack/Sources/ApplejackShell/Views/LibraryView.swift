import SwiftUI

public struct LibraryView: View {
    let models: ShellModels
    @Bindable var model: LibraryViewModel
    @State private var path: [UInt32] = []

    public init(models: ShellModels) {
        self.models = models
        _model = Bindable(models.library)
    }

    public var body: some View {
        NavigationStack(path: $path) {
            FlowStateView(model.library, retry: { Task { await model.load() } }) { _ in
                titles
            }
            .searchable(text: $model.query, prompt: "Name or appid")
            .navigationDestination(for: UInt32.self) { appid in
                GameDetailView(models: models, appid: appid)
            }
            .navigationTitle("Library")
            .toolbar {
                Button("Refresh") { Task { await model.load(refresh: true) } }
            }
        }
        .task {
            if path.isEmpty, let detail = models.libraryDetail {
                path = [detail]
            }
            await model.load()
        }
        .onChange(of: path) { _, next in models.libraryDetail = next.last }
    }

    @ViewBuilder
    private var titles: some View {
        let apps = model.filteredApps
        VStack(spacing: 0) {
            if apps.isEmpty, model.matchCount != nil {
                ContentUnavailableView.search(text: model.query)
            } else {
                List(apps, selection: $model.selection) { entry in
                    row(entry)
                }
            }
            if let count = model.matchCount {
                Divider()
                Text(count)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 6)
                    .accessibilityIdentifier("library.matchCount")
            }
        }
    }

    private func row(_ entry: LibraryEntry) -> some View {
        HStack {
            NavigationLink(value: entry.appid) {
                VStack(alignment: .leading) {
                    Text(entry.title)
                    Text("App \(entry.appid) · \(entry.drm.rawValue) DRM").font(.caption).foregroundStyle(.secondary)
                }
            }
            .accessibilityLabel(entry.title)
            .accessibilityIdentifier("library.app.\(entry.appid)")
            Spacer()
            installColumn(entry)
        }
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
