// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "FiveVectorsSearch",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../.."),
    ],
    targets: [
        .executableTarget(
            name: "FiveVectorsSearch",
            dependencies: [
                .product(name: "ZeppelinEmbed", package: "zeppelin-embed"),
            ]
        ),
    ]
)
