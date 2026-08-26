// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "MacDemo",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../../swift/ZeppelinEmbed"),
    ],
    targets: [
        .executableTarget(name: "MacDemo", dependencies: ["ZeppelinEmbed"]),
    ]
)
