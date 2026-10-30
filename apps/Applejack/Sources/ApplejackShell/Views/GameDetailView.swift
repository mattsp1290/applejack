import SwiftUI
import UniformTypeIdentifiers

public struct GameDetailView: View {
    @State private var model: GameDetailViewModel
    @State private var choosingPreset = false

    public init(app: AppModel, appid: UInt32) {
        _model = State(initialValue: GameDetailViewModel(app: app, appid: appid))
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            FlowStateView(model.config, retry: { Task { await model.load() } }) { config in
                Form {
                    Section("Configuration") {
                        ForEach(ConfigKey.allCases, id: \.rawValue) { key in
                            ConfigFieldRow(key: key, value: config.value(key)) { value in
                                Task { await model.set(key, to: value) }
                            }
                        }
                    }
                    Section("Extra environment") {
                        if config.extraEnv.isEmpty {
                            Text("None").foregroundStyle(.secondary)
                        }
                        ForEach(config.extraEnv.keys.sorted(), id: \.self) { name in
                            LabeledContent(name, value: config.extraEnv[name] ?? "")
                        }
                    }
                }
                .formStyle(.grouped)
            }
            saveStatus
            importStatus
        }
        .padding()
        .navigationTitle("App \(model.appid)")
        .toolbar {
            Button("Import preset…") { choosingPreset = true }
        }
        .fileImporter(isPresented: $choosingPreset, allowedContentTypes: [.json]) { result in
            if case .success(let url) = result {
                Task { await model.importPreset(at: url.path) }
            }
        }
        .alert("This preset changes how the game is launched", isPresented: confirmationShown) {
            Button("Apply overrides") { Task { await model.confirmLaunchOverrides() } }
            Button("Cancel", role: .cancel) { model.declineLaunchOverrides() }
        } message: {
            Text(model.pendingConfirmation.map { "\($0.field) = \($0.value)" }.joined(separator: "\n"))
        }
        .task { await model.load() }
    }

    private var confirmationShown: Binding<Bool> {
        Binding(
            get: { !model.pendingConfirmation.isEmpty },
            set: { shown in if !shown { model.declineLaunchOverrides() } }
        )
    }

    @ViewBuilder
    private var saveStatus: some View {
        switch model.lastSave {
        case .idle, .ready: EmptyView()
        case .loading: ProgressView().controlSize(.small)
        case .unavailable(let reason): Text("Saving is not available in this build: \(reason.message)").foregroundStyle(.secondary)
        case .failed(let error): Text(describe(error)).foregroundStyle(.red)
        }
    }

    @ViewBuilder
    private var importStatus: some View {
        FlowStateView(model.importState, idle: "") { result in
            if result.applied {
                Text("Preset applied: \(result.accepted.count) accepted, \(result.rejected.count) rejected.")
                ForEach(result.rejected) { item in
                    Text("Rejected \(item.field): \(item.reason)").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// One editable `config.set` key, rendered by its kind.
struct ConfigFieldRow: View {
    let key: ConfigKey
    let value: JSONValue
    let onSet: (JSONValue) -> Void
    @State private var text = ""

    var body: some View {
        switch key.kind {
        case .toggle:
            Toggle(key.rawValue, isOn: Binding(get: { value.boolValue ?? false }, set: { onSet(.bool($0)) }))
        case .choice(let options):
            Picker(key.rawValue, selection: Binding(get: { value.stringValue ?? "" }, set: { onSet($0.isEmpty ? .null : .string($0)) })) {
                Text("unset").tag("")
                ForEach(options, id: \.self) { Text($0).tag($0) }
            }
        case .text:
            TextField(key.rawValue, text: $text, prompt: Text("unset"))
                .onAppear { text = value.stringValue ?? "" }
                .onChange(of: value) { _, newValue in text = newValue.stringValue ?? "" }
                .onSubmit { onSet(text.isEmpty ? .null : .string(text)) }
        }
    }
}
