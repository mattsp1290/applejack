import AppKit
import ApplejackShell
import SwiftUI

// `main.swift` runs top-level code, so the `App` is started by hand instead
// of with `@main`.
struct ApplejackApp: App {
    @State private var models = ShellModels(
        app: AppModel(),
        initialSection: ProcessInfo.processInfo.environment["APPLEJACK_SECTION"].flatMap(ShellSection.init(rawValue:))
    )

    var body: some Scene {
        WindowGroup("Applejack") {
            RootView(models: models).frame(minWidth: 900, minHeight: 600)
        }
    }
}

// Outside a bundle (`swift run`) the process has no Info.plist to make it a
// regular app; inside the bundle this is a no-op.
NSApplication.shared.setActivationPolicy(.regular)
ApplejackApp.main()
