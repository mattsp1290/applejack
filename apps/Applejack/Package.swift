// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "Applejack",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "ApplejackShell", targets: ["ApplejackShell"]),
        .executable(name: "ApplejackApp", targets: ["ApplejackApp"]),
    ],
    targets: [
        .target(name: "ApplejackShell"),
        .executableTarget(
            name: "ApplejackApp",
            dependencies: ["ApplejackShell"]
        ),
        .testTarget(
            name: "ApplejackShellTests",
            dependencies: ["ApplejackShell"]
        ),
    ],
    swiftLanguageModes: [.v6]
)
