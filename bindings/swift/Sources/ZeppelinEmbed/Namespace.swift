import CZeppelinEmbed
import Foundation

public struct VectorSpace: Sendable {
    public var dimensions: UInt32
    public var normalization: VectorNormalization

    public init(dimensions: UInt32, normalization: VectorNormalization) {
        self.dimensions = dimensions
        self.normalization = normalization
    }
}

public struct NamespaceSpec: Sendable {
    public var attributes: [AttributeDefinition]
    public var vectorSpace: VectorSpace?
    public var epoch: Epoch?

    public init(
        attributes: [AttributeDefinition],
        vectorSpace: VectorSpace?,
        epoch: Epoch? = nil
    ) {
        self.attributes = attributes
        self.vectorSpace = vectorSpace
        self.epoch = epoch
    }
}

struct NamespaceReopenConfiguration: Sendable {
    var root: URL
    var name: String
    var spec: NamespaceSpec
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public static func openNamespace(
        root: URL,
        name: String,
        spec: NamespaceSpec,
        options: OpenOptions = OpenOptions()
    ) async throws -> ZeppelinStore {
        let namespace = NamespaceReopenConfiguration(root: root, name: name, spec: spec)
        return try await open(
            configuration: ReopenConfiguration(
                path: root.appendingPathComponent(name, isDirectory: true),
                options: options,
                epoch: spec.epoch,
                namespace: namespace
            )
        )
    }

    public static func listNamespaces(root: URL) async throws -> [String] {
        try await runBlocking {
            let rootBytes = Array(root.path.utf8)
            return try rootBytes.withUnsafeBufferPointer { rootBuffer in
                var request = ZeNamespaceListRequest()
                request.abi_size = abiSize(ZeNamespaceListRequest.self)
                request.root = rootBuffer.baseAddress
                request.root_len = rootBytes.count
                var result = ZeNamespaceListResult()
                result.abi_size = abiSize(ZeNamespaceListResult.self)
                defer { _ = ze_namespace_list_result_free(&result) }
                try checkZeppelin(ze_namespace_list(&request, &result))
                guard result.entry_count > 0 else {
                    return []
                }
                guard let entries = result.entries else {
                    throw ZeppelinError.internalError
                }
                return try UnsafeBufferPointer(start: entries, count: result.entry_count).map {
                    entry in
                    guard entry.name_len == 0 || entry.name != nil,
                        let name = String(
                            bytes: UnsafeBufferPointer(start: entry.name, count: entry.name_len),
                            encoding: .utf8
                        )
                    else {
                        throw ZeppelinError.internalError
                    }
                    return name
                }
            }
        }
    }

    static func openNamespaceHandle(
        _ configuration: NamespaceReopenConfiguration,
        options: OpenOptions
    ) throws -> UInt64 {
        let rootBytes = Array(configuration.root.path.utf8)
        let nameBytes = Array(configuration.name.utf8)
        let nameFields = configuration.spec.attributes.map { Array($0.name.utf8) }
        let nameOffsets = offsets(nameFields.map(\.count))
        let attributeNameBytes = nameFields.flatMap { $0 }

        return try rootBytes.withUnsafeBufferPointer { rootBuffer in
            try nameBytes.withUnsafeBufferPointer { nameBuffer in
                try attributeNameBytes.withUnsafeBufferPointer { attributeNameBuffer in
                    let definitions = configuration.spec.attributes.enumerated().map {
                        index, definition in
                        var raw = ZeAttributeDefinition()
                        raw.attribute_id = definition.id
                        raw.name = pointer(
                            attributeNameBuffer.baseAddress,
                            offset: nameOffsets[index],
                            count: nameFields[index].count
                        )
                        raw.name_len = nameFields[index].count
                        raw.attribute_type = definition.type.rawValue
                        raw.nullable = definition.nullable ? 1 : 0
                        return raw
                    }
                    return try definitions.withUnsafeBufferPointer { definitionBuffer in
                        func invoke(_ epoch: UnsafePointer<ZeEpochRequest>?) throws -> UInt64 {
                            var spec = ZeNamespaceSpec()
                            spec.abi_size = abiSize(ZeNamespaceSpec.self)
                            spec.attributes = definitionBuffer.baseAddress
                            spec.attribute_count = definitions.count
                            spec.has_vector_space = configuration.spec.vectorSpace == nil ? 0 : 1
                            spec.dimensions = configuration.spec.vectorSpace?.dimensions ?? 0
                            spec.normalization =
                                configuration.spec.vectorSpace?.normalization.rawValue ?? 0
                            spec.epoch = epoch
                            return try withUnsafePointer(to: &spec) { specPointer in
                                var open = ZeOpenRequest()
                                open.abi_size = abiSize(ZeOpenRequest.self)
                                open.access_mode = options.accessMode.rawValue
                                open.durability_mode = options.durabilityMode.rawValue
                                open.commit_tier = options.commitTier.rawValue
                                open.reader_drain_timeout_ms =
                                    options.readerDrainTimeoutMilliseconds
                                open.max_resident_bytes = options.maxResidentBytes
                                open.max_temp_bytes = options.maxTemporaryBytes

                                var request = ZeNamespaceOpenRequest()
                                request.abi_size = abiSize(ZeNamespaceOpenRequest.self)
                                request.root = rootBuffer.baseAddress
                                request.root_len = rootBytes.count
                                request.name = nameBuffer.baseAddress
                                request.name_len = nameBytes.count
                                request.open = open
                                request.spec = specPointer
                                var handle: UInt64 = 0
                                try checkZeppelin(ze_namespace_open(&request, &handle))
                                return handle
                            }
                        }

                        if let epoch = configuration.spec.epoch {
                            return try withEpochRequest(epoch) { try invoke($0) }
                        }
                        return try invoke(nil)
                    }
                }
            }
        }
    }
}
