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

/// Lists the pads the GameController framework sees and refreshes on
/// connect and disconnect. No core call: the shell owns controller status.
@Observable @MainActor
public final class ControllersViewModel {
    public typealias Provider = @MainActor () -> [ControllerStatus]

    public private(set) var controllers: [ControllerStatus] = []
    private let provider: Provider
    private var observers: [any NSObjectProtocol] = []

    public init(provider: @escaping Provider = ControllersViewModel.connectedControllers) {
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

    public func refresh() {
        controllers = provider()
    }

    /// Refreshes now and on every connect or disconnect until `stop()`.
    public func start() {
        refresh()
        guard observers.isEmpty else { return }
        for name in [Notification.Name.GCControllerDidConnect, .GCControllerDidDisconnect] {
            let observer = NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in self?.refresh() }
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
