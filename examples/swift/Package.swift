// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "FiveVectorsSearch",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../../swift/ZeppelinEmbed"),
    ],
    targets: [
        .executableTarget(name: "FiveVectorsSearch", dependencies: ["ZeppelinEmbed"]),
    ]
)
