import SwiftUI

public struct ControllersView: View {
    let model: ControllersViewModel

    public init(model: ControllersViewModel) {
        self.model = model
    }

    public var body: some View {
        Group {
            if model.controllers.isEmpty {
                ContentUnavailableView("No controllers connected", systemImage: "gamecontroller", description: Text("Pads appear here as macOS sees them; input reaches the game through Wine."))
            } else {
                List(model.controllers) { controller in
                    VStack(alignment: .leading) {
                        Text(controller.vendorName)
                        Text("\(controller.productCategory) · \(controller.hasExtendedGamepad ? "extended gamepad" : "no extended gamepad profile") · \(controller.isAttachedToDevice ? "wired" : "wireless")")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
        }
        .navigationTitle("Controllers")
        .toolbar {
            Button("Refresh") { model.refresh() }
        }
        .onAppear { model.start() }
        .onDisappear { model.stop() }
    }
}
