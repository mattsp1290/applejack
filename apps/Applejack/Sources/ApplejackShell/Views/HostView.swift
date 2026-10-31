import SwiftUI

public struct HostView: View {
    let model: HostViewModel

    public init(model: HostViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading) {
            FlowStateView(model.facts, retry: { Task { await model.load() } }) { facts in
                Form {
                    LabeledContent("macOS", value: facts.macosVersion)
                    LabeledContent("Architecture", value: facts.arch ?? "unknown")
                    LabeledContent("Rosetta", value: facts.rosettaPresent.map { $0 ? "present" : "absent" } ?? "unknown")
                    LabeledContent("Free disk", value: facts.freeDiskBytes.map { ByteCountFormatter.string(fromByteCount: Int64(clamping: $0), countStyle: .file) } ?? "unknown")
                    LabeledContent("Screen Recording", value: facts.screenRecording.rawValue)
                    LabeledContent("Developer ID identities", value: facts.developerIdIdentities.map(String.init) ?? "unknown")
                }
                .formStyle(.grouped)
            }
            Spacer()
        }
        .padding()
        .navigationTitle("Host")
        .toolbar {
            Button("Refresh") { Task { await model.load() } }
        }
        .task { await model.load() }
    }
}
