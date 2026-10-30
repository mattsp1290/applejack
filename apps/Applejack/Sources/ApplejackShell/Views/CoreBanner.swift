import SwiftUI

/// The core's lifecycle, above every view: hidden while it runs, a restart
/// action after an exit, and the final states the restart cap leaves.
public struct CoreBanner: View {
    let app: AppModel

    public init(app: AppModel) {
        self.app = app
    }

    public var body: some View {
        switch app.coreStatus {
        case .running:
            EmptyView()
        case .starting:
            banner(systemImage: "circle.dotted", text: "Starting the core…", tint: .secondary)
        case .missing:
            banner(
                systemImage: "questionmark.app",
                text: "applejack-core was not found beside the app and \(CoreLocator.environmentVariable) is not set.",
                tint: .orange
            )
        case .exited(let status):
            banner(systemImage: "bolt.slash", text: "The core exited (status \(status)).", tint: .red) {
                Button("Restart") { Task { await app.restart() } }
            }
        case .failed(let error):
            banner(systemImage: "xmark.octagon", text: describe(error), tint: .red)
        }
    }

    private func banner(
        systemImage: String, text: String, tint: Color, @ViewBuilder action: () -> some View = { EmptyView() }
    ) -> some View {
        HStack {
            Label(text, systemImage: systemImage)
            Spacer()
            action()
        }
        .padding(10)
        .background(tint.opacity(0.12))
        .overlay(alignment: .bottom) { Divider() }
    }
}
