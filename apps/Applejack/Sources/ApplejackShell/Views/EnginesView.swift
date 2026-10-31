import SwiftUI

public struct EnginesView: View {
    let model: EnginesViewModel

    public init(model: EnginesViewModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            FlowStateView(model.engines, retry: { Task { await model.load() } }) { list in
                List(list.engines) { engine in
                    HStack {
                        VStack(alignment: .leading) {
                            Text("\(engine.engineId) \(engine.version)")
                            Text(engine.active ? "active" : engine.installed ? "installed" : "not installed")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        if !engine.installed {
                            Button("Install") { Task { await model.install(engine.engineId) } }
                        } else if !engine.active {
                            Button("Use") { Task { await model.use(engine.engineId, version: engine.version) } }
                        }
                    }
                }
            }
            JobProgressView(model.install, title: "Engine install")
            HStack {
                Button("Install rosetta-wine") { Task { await model.install("rosetta-wine") } }
                Button("Doctor") { Task { await model.runDoctor() } }
                Button("Probe CPUID") { Task { await model.probeCpuid() } }
            }
            FlowStateView(model.use, idle: "") { _ in Text("Engine switched.") }
            FlowStateView(model.doctor, idle: "") { report in
                VStack(alignment: .leading) {
                    Label(report.runnable ? "Runnable" : "Not runnable", systemImage: report.runnable ? "checkmark.seal" : "xmark.seal")
                    ForEach(Array(report.findings.enumerated()), id: \.offset) { _, finding in
                        Text("\(finding.severity.rawValue): \(finding.message)").font(.caption)
                    }
                }
            }
            FlowStateView(model.cpuid, idle: "") { result in
                Text("CPUID: \(result.display)").font(.caption.monospaced())
            }
            Spacer()
        }
        .padding()
        .navigationTitle("Engines")
        .task { await model.load() }
    }
}
