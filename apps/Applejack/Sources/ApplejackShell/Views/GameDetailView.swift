import SwiftUI
import UniformTypeIdentifiers

public struct GameDetailView: View {
    let models: ShellModels
    @State private var model: GameDetailViewModel
    @State private var choosingPreset = false
    @State private var isShown = false

    public init(models: ShellModels, appid: UInt32) {
        self.models = models
        _model = State(initialValue: GameDetailViewModel(app: models.app, appid: appid))
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
            runStatus
        }
        .padding()
        .navigationTitle("App \(model.appid)")
        .toolbar {
            Button("Run") { Task { await runAndFollowLog() } }
                .disabled(model.runState == .loading)
            Button("Import preset…") { choosingPreset = true }
        }
        .onAppear { isShown = true }
        .onDisappear { isShown = false }
        .fileImporter(isPresented: $choosingPreset, allowedContentTypes: [.json]) { result in
            if case .success(let url) = result {
                Task { await model.importPreset(at: url.path) }
            }
        }
        .alert("This preset changes how the game is launched", isPresented: confirmationShown, presenting: model.pendingImport) { pending in
            // `pending` is the value the alert was presented with; the
            // binding below has already declined by the time this task runs.
            Button("Apply overrides") { Task { await model.confirmLaunchOverrides(path: pending.path) } }
            Button("Cancel", role: .cancel) { model.declineLaunchOverrides() }
        } message: { pending in
            Text(planSummary(pending))
        }
        .task { await model.load() }
    }

    private var confirmationShown: Binding<Bool> {
        Binding(
            get: { model.pendingImport != nil },
            set: { shown in if !shown { model.declineLaunchOverrides() } }
        )
    }

    private func planSummary(_ pending: GameDetailViewModel.PendingImport) -> String {
        var lines = model.presetMismatch(presetAppid: pending.presetAppid).map { [$0] } ?? []
        lines.append(contentsOf: pending.confirmations.map { "\($0.field) = \($0.value)" })
        if !pending.rejected.isEmpty {
            lines.append("")
            lines.append(contentsOf: pending.rejected.map { "Rejected \($0.field): \($0.reason)" })
        }
        return lines.joined(separator: "\n")
    }

    /// `app.run`, then Logs tails the session's log when the core named one,
    /// unless the user left this view while the run was in flight.
    private func runAndFollowLog() async {
        await model.run()
        guard isShown, let path = model.runState.value?.logPath else { return }
        models.logs.follow(path)
        models.section = .logs
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
            VStack(alignment: .leading, spacing: 2) {
                if let mismatch = model.presetMismatch(presetAppid: result.presetAppid) {
                    Text(mismatch).foregroundStyle(.orange)
                }
                if result.applied {
                    Text("Preset applied: \(result.accepted.count) accepted, \(result.rejected.count) rejected.")
                } else if !result.confirmations.isEmpty, model.pendingImport == nil {
                    Text("Preset not applied: launch overrides declined (\(result.accepted.count) accepted, \(result.rejected.count) rejected).")
                } else {
                    Text("Preset not applied: \(result.accepted.count) accepted, \(result.rejected.count) rejected, \(result.confirmations.count) awaiting confirmation.")
                }
                ForEach(result.accepted) { item in
                    Text("Accepted \(item.field) = \(item.value)").font(.caption).foregroundStyle(.secondary)
                }
                ForEach(result.rejected) { item in
                    Text("Rejected \(item.field): \(item.reason)").font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }

    @ViewBuilder
    private var runStatus: some View {
        FlowStateView(model.runState, idle: "", retry: { Task { await runAndFollowLog() } }) { result in
            if let path = result.logPath {
                Text("Launched; the log is at \(path).").foregroundStyle(.secondary)
            } else {
                Text("Launched.").foregroundStyle(.secondary)
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
    @FocusState private var focused: Bool

    var body: some View {
        switch key.kind {
        case .toggle:
            HStack {
                Toggle(key.rawValue, isOn: Binding(get: { value.boolValue ?? false }, set: { onSet(.bool($0)) }))
                if value == .null {
                    Text("unset").foregroundStyle(.secondary)
                }
            }
        case .choice(let options):
            Picker(key.rawValue, selection: Binding(get: { value.stringValue ?? "" }, set: { onSet($0.isEmpty ? .null : .string($0)) })) {
                Text("unset").tag("")
                ForEach(options, id: \.self) { Text($0).tag($0) }
            }
        case .text:
            TextField(key.rawValue, text: $text, prompt: Text("unset"))
                .focused($focused)
                .onAppear { text = value.stringValue ?? "" }
                .onChange(of: value) { _, newValue in text = newValue.stringValue ?? "" }
                .onChange(of: focused) { _, isFocused in if !isFocused { commit() } }
                .onSubmit(commit)
        }
    }

    /// Saves the field when its text differs from the stored value, so a
    /// focus change without an edit sends nothing.
    private func commit() {
        let next: JSONValue = text.isEmpty ? .null : .string(text)
        if next != value {
            onSet(next)
        }
    }
}
