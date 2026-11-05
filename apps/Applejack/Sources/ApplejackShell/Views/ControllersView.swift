import SwiftUI

public struct ControllersView: View {
    let model: ControllersViewModel

    public init(model: ControllersViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            Section {
                if model.controllers.isEmpty {
                    Text("No controllers connected.").foregroundStyle(.secondary)
                } else {
                    ForEach(model.controllers) { controller in
                        VStack(alignment: .leading) {
                            Text(controller.vendorName)
                            Text("\(controller.productCategory) · \(controller.hasExtendedGamepad ? "extended gamepad" : "no extended gamepad profile") · \(controller.isAttachedToDevice ? "wired" : "wireless")")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            } header: {
                Text("What macOS sees")
            } footer: {
                Text("Pads as the GameController framework reports them; input reaches the game through Wine.")
            }
            Section {
                FlowStateView(model.inventory, idle: "Not read yet.", retry: { Task { await model.loadInventory() } }) { pads in
                    if pads.isEmpty {
                        Text("No pads paired or plugged in.").foregroundStyle(.secondary)
                    }
                    ForEach(Array(pads.enumerated()), id: \.offset) { _, pad in
                        VStack(alignment: .leading) {
                            Text(pad.name)
                            Text("\(pad.transport.rawValue) · \(pad.vendorId):\(pad.productId) · \(pad.kind.rawValue) · \(pad.connected ? "connected" : "not connected") · hidraw default \(pad.kind.hidrawDefaultEnabled ? "enabled" : "disabled")")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    if let rule = model.hidrawDefault {
                        Text(rule.summary)
                    }
                }
            } header: {
                Text("What the core will act on")
            } footer: {
                Text("The inventory `host.doctor` reports and the hidraw default it gives a game whose hidraw_enabled is unset; a stored value always wins.")
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Controllers")
        .toolbar {
            Button("Refresh") { Task { await model.reload() } }
        }
        .onAppear { model.start() }
        .onDisappear { model.stop() }
        .task { await model.loadInventory() }
    }
}
