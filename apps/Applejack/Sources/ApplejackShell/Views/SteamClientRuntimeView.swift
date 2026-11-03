import SwiftUI

/// `runtime.steam.status`, the four runtime jobs with every progress line
/// they report, and the `steam-client` prefix's snapshots.
public struct SteamClientRuntimeView: View {
    let model: SteamClientRuntimeViewModel

    public init(model: SteamClientRuntimeViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Steam client").font(.headline)
            FlowStateView(model.status, idle: "Steam client state unknown.", retry: { Task { await model.load() } }) { status in
                VStack(alignment: .leading, spacing: 2) {
                    LabeledContent("State", value: status.state.rawValue)
                    LabeledContent("Build", value: status.buildId ?? "none")
                    LabeledContent("Running", value: status.running ? "yes" : "no")
                    LabeledContent("Snapshots", value: String(status.snapshots))
                    if !status.detail.isEmpty {
                        Text(status.detail).font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            HStack {
                ForEach([SteamClientRuntimeViewModel.Action.install, .update, .repair, .reinstall, .stop], id: \.rawValue) { action in
                    Button(action.rawValue) { Task { await model.perform(action) } }
                        .disabled(!model.canPerform(action))
                        .accessibilityIdentifier("runtime.\(action.rawValue.lowercased())")
                }
                Button("Refresh") { Task { await model.load() } }
                    .disabled(model.isBusy)
            }
            if let action = model.action {
                JobProgressView(model.job, title: action.rawValue)
                ForEach(model.progress) { line in
                    Text(line.text).font(.caption).foregroundStyle(line.phase == "start" ? .orange : .secondary)
                        .textSelection(.enabled)
                }
            }
            PrefixSnapshotsView(model: model.snapshots)
        }
    }
}

/// Snapshot, Restore by timestamp, and Reset for one prefix.
public struct PrefixSnapshotsView: View {
    @Bindable var model: PrefixSnapshotsViewModel
    @State private var keepUserData = true

    public init(model: PrefixSnapshotsViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Prefix \(model.prefix)").font(.subheadline)
            HStack {
                Button("Snapshot") { Task { await model.takeSnapshot() } }
                Toggle("Keep user data", isOn: $keepUserData)
                    .help("Reset takes a safety snapshot and rebuilds the prefix from scratch; on, drive_c/users is copied back from that snapshot. Everything else is new either way.")
                Button("Reset") { Task { await model.resetPrefix(keepUserData: keepUserData) } }
            }
            .disabled(model.isBusy)
            HStack {
                TextField("Snapshot timestamp", text: $model.restoreTimestamp, prompt: Text("20261031T120000Z"))
                    .frame(maxWidth: 200)
                if !model.knownTimestamps.isEmpty {
                    Picker("Known", selection: $model.restoreTimestamp) {
                        Text("Known this session").tag("")
                        ForEach(model.knownTimestamps, id: \.self) { Text($0).tag($0) }
                    }
                    .frame(maxWidth: 240)
                }
                Button("Restore") { Task { await model.restoreSelected() } }
                    .disabled(model.restoreTimestamp.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
            .disabled(model.isBusy)
            FlowStateView(model.snapshot, idle: "") { made in
                Text("Snapshot \(made.timestamp) taken.").font(.caption)
            }
            FlowStateView(model.restore, idle: "") { safety in
                Text("Restored; the replaced state is snapshot \(safety.timestamp).").font(.caption)
            }
            JobProgressView(model.reset, title: "Reset")
            if let timestamp = model.resetFailureSnapshot {
                Button("Restore \(timestamp)") { Task { await model.restore(timestamp: timestamp) } }
            }
        }
    }
}
