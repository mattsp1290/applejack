import SwiftUI
import UniformTypeIdentifiers

public struct LogsView: View {
    let model: LogsViewModel
    @State private var choosingFile = false

    public init(model: LogsViewModel) {
        self.model = model
    }

    public var body: some View {
        HSplitView {
            VStack(alignment: .leading) {
                FlowStateView(model.sessions, retry: { Task { await model.load() } }) { list in
                    List(list.sessions, selection: Binding(get: { model.selected?.id }, set: { id in
                        if let session = list.sessions.first(where: { $0.id == id }) { model.select(session) }
                    })) { session in
                        VStack(alignment: .leading) {
                            Text("App \(session.appid) · session \(session.id)")
                            Text(session.startedAt).font(.caption).foregroundStyle(.secondary)
                        }
                        .tag(session.id)
                    }
                }
                Button("Follow a log file…") { choosingFile = true }
            }
            .frame(minWidth: 220)
            .padding()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    if let path = model.tailedPath {
                        Text(path).font(.caption).foregroundStyle(.secondary)
                    }
                    ForEach(Array(model.lines.enumerated()), id: \.offset) { _, line in
                        Text(line).font(.body.monospaced()).textSelection(.enabled)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding()
            }
        }
        .navigationTitle("Logs")
        .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.log, .plainText, .data]) { result in
            if case .success(let url) = result {
                model.follow(url.path)
            }
        }
        .task { await model.load() }
    }
}
