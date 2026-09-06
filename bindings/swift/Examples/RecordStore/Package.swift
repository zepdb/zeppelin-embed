// swift-tools-version: 5.10

import PackageDescription

let package = Package(
    name: "RecordStore",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(name: "zeppelin-embed", path: "../../../.."),
    ],
    targets: [
        .executableTarget(
            name: "RecordStore",
            dependencies: [
                .product(name: "ZeppelinEmbed", package: "zeppelin-embed"),
            ]
        ),
    ]
)
