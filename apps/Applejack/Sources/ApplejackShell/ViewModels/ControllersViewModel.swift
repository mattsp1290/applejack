import Foundation
import GameController
import Observation

/// One connected pad as the status view lists it. Display only: input
/// reaches the game through Wine's own HID path.
public struct ControllerStatus: Equatable, Sendable, Identifiable {
    public var id: String
    public var vendorName: String
    public var productCategory: String
    public var hasExtendedGamepad: Bool
    public var isAttachedToDevice: Bool

    public init(id: String, vendorName: String, productCategory: String, hasExtendedGamepad: Bool, isAttachedToDevice: Bool) {
        self.id = id
        self.vendorName = vendorName
        self.productCategory = productCategory
        self.hasExtendedGamepad = hasExtendedGamepad
        self.isAttachedToDevice = isAttachedToDevice
    }
}

/// Two lists: the pads the GameController framework sees (what macOS
/// sees, refreshed on connect and disconnect) and the core's inventory from
/// `host.doctor` (what a run acts on, with the hidraw default it gives a
/// title whose `hidraw_enabled` is unset).
@Observable @MainActor
public final class ControllersViewModel {
    public typealias Provider = @MainActor () -> [ControllerStatus]

    public private(set) var controllers: [ControllerStatus] = []
    /// `host.doctor`'s `controllers` and `controllers_unavailable`; a core
    /// that cannot be reached or refuses leaves the failure here, never an
    /// empty list.
    public private(set) var inventory: FlowState<ControllerInventory> = .idle
    private let app: AppModel
    private let provider: Provider
    private var observers: [any NSObjectProtocol] = []

    public init(app: AppModel, provider: @escaping Provider = ControllersViewModel.connectedControllers) {
        self.app = app
        self.provider = provider
    }

    public static func connectedControllers() -> [ControllerStatus] {
        GCController.controllers().enumerated().map { index, controller in
            ControllerStatus(
                id: "\(index):\(controller.vendorName ?? "unknown")",
                vendorName: controller.vendorName ?? "Unknown controller",
                productCategory: controller.productCategory,
                hasExtendedGamepad: controller.extendedGamepad != nil,
                isAttachedToDevice: controller.isAttachedToDevice
            )
        }
    }

    /// The rule the inventory read last gives; `nil` until one is ready,
    /// and `nil` while the inventory is unavailable with no listed Sony pad
    /// to decide it (the view shows `inventory.unavailableLine` in its
    /// place).
    public var hidrawDefault: HidrawDefault? {
        inventory.value.flatMap(HidrawDefault.decide)
    }

    /// The GameController list, now.
    public func refresh() {
        controllers = provider()
    }

    /// `host.doctor`, for the inventory.
    public func loadInventory() async {
        inventory = .loading
        if let next = await app.run({ try await $0.hostDoctor() }) {
            inventory = next.map(\.controllerInventory)
        }
    }

    /// Both lists: what Refresh and a banner Restart do.
    public func reload() async {
        refresh()
        await loadInventory()
    }

    /// Refreshes the GameController list now and both lists on every
    /// connect or disconnect until `stop()`.
    public func start() {
        refresh()
        guard observers.isEmpty else { return }
        for name in [Notification.Name.GCControllerDidConnect, .GCControllerDidDisconnect] {
            let observer = NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in await self?.reload() }
            }
            observers.append(observer)
        }
    }

    public func stop() {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers = []
    }
}
