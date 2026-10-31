import Foundation
import Observation

/// `host.doctor`, the one method the core answers today.
@Observable @MainActor
public final class HostViewModel {
    public private(set) var facts: FlowState<HostFacts> = .idle
    private let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public func load() async {
        facts = .loading
        if let next = await app.run({ try await $0.hostDoctor() }) {
            facts = next
        }
    }
}
