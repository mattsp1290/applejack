import Foundation
import Observation

/// `engine.list`, `engine.install` with progress, `engine.use`,
/// `engine.doctor`, and `engine.probe_cpuid`.
@Observable @MainActor
public final class EnginesViewModel {
    public private(set) var engines: FlowState<EngineListResult> = .idle
    public private(set) var install: FlowState<JobSnapshot> = .idle
    public private(set) var use: FlowState<JSONValue> = .idle
    public private(set) var doctor: FlowState<DoctorReport> = .idle
    public private(set) var cpuid: FlowState<JSONValue> = .idle
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public func load() async {
        engines = .loading
        if let next = await app.run({ try await $0.call("engine.list", params: EmptyParams()) as EngineListResult }) {
            engines = next
        }
    }

    public func install(_ engineId: String) async {
        await app.runJob("engine.install", params: EngineInstallParams(engineId: engineId)) { install = $0 }
        if case .ready(let snapshot) = install, snapshot.finished {
            await load()
        }
    }

    public func use(_ engineId: String, version: String) async {
        use = .loading
        if let next = await app.run({ try await $0.call("engine.use", params: EngineUseParams(engineId: engineId, version: version)) as JSONValue }) {
            use = next
            if case .ready = next {
                await load()
            }
        }
    }

    public func runDoctor() async {
        doctor = .loading
        if let next = await app.run({ try await $0.call("engine.doctor", params: EmptyParams()) as DoctorReport }) {
            doctor = next
        }
    }

    public func probeCpuid() async {
        cpuid = .loading
        if let next = await app.run({ try await $0.call("engine.probe_cpuid", params: EmptyParams()) as JSONValue }) {
            cpuid = next
        }
    }
}
