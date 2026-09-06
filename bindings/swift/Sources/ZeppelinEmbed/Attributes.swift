import CZeppelinEmbed
import Foundation

public enum AttributeType: Int32, Sendable {
    case u64 = 1
    case i64 = 2
    case f64 = 3
    case bool = 4
    case dictionaryString = 5
    case rawString = 6
}

public struct AttributeDefinition: Sendable {
    public var id: UInt32
    public var name: String
    public var type: AttributeType
    public var nullable: Bool

    public init(id: UInt32, name: String, type: AttributeType, nullable: Bool) {
        self.id = id
        self.name = name
        self.type = type
        self.nullable = nullable
    }
}

public enum AttributeValue: Sendable, Equatable {
    case null
    case u64(UInt64)
    case i64(Int64)
    case f64(Double)
    case bool(Bool)
    case string(String)
}

struct CAttributeValueRecord: Sendable {
    var attributeID: UInt32
    var valueType: Int32 = 0
    var u64Value: UInt64 = 0
    var i64Value: Int64 = 0
    var f64Value: Double = 0
    var boolValue: UInt32 = 0
    var stringOffset: Int = 0
    var stringLength: Int = 0

    init(attributeID: UInt32, value: AttributeValue, strings: inout [UInt8]) {
        self.attributeID = attributeID
        switch value {
        case .null:
            break
        case .u64(let value):
            valueType = 1
            u64Value = value
        case .i64(let value):
            valueType = 2
            i64Value = value
        case .f64(let value):
            valueType = 3
            f64Value = value
        case .bool(let value):
            valueType = 4
            boolValue = value ? 1 : 0
        case .string(let value):
            valueType = 5
            stringOffset = strings.count
            let bytes = Array(value.utf8)
            stringLength = bytes.count
            strings.append(contentsOf: bytes)
        }
    }

    func rawValue(stringBase: UnsafePointer<UInt8>?) -> ZeAttributeValue {
        var raw = ZeAttributeValue()
        raw.attribute_id = attributeID
        raw.value_type = valueType
        raw.u64_value = u64Value
        raw.i64_value = i64Value
        raw.f64_value = f64Value
        raw.bool_value = boolValue
        raw.string_value = ZeppelinStore.pointer(
            stringBase,
            offset: stringOffset,
            count: stringLength
        )
        raw.string_len = stringLength
        return raw
    }
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public func upsert(_ documents: [IngestDocument]) async throws -> MutationReport {
        let current = try openHandle()
        let vectors = documents.flatMap(\.vector)
        let metadata = documents.flatMap { Array($0.metadata) }
        let texts = documents.flatMap { $0.text.map { Array($0.utf8) } ?? [] }
        let vectorOffsets = Self.offsets(documents.map { $0.vector.count })
        let metadataOffsets = Self.offsets(documents.map { $0.metadata.count })
        let textLengths = documents.map { $0.text.map { $0.utf8.count } ?? 0 }
        let textOffsets = Self.offsets(textLengths)
        let sortedAttributes = documents.map { document in
            document.attributes.sorted { $0.key < $1.key }
        }
        let attributeOffsets = Self.offsets(sortedAttributes.map(\.count))
        var stringBytes: [UInt8] = []
        let attributeRecords = sortedAttributes.flatMap { attributes in
            attributes.map { id, value in
                CAttributeValueRecord(attributeID: id, value: value, strings: &stringBytes)
            }
        }

        return try stringBytes.withUnsafeBufferPointer { stringBuffer in
            let rawAttributes = attributeRecords.map {
                $0.rawValue(stringBase: stringBuffer.baseAddress)
            }
            return try rawAttributes.withUnsafeBufferPointer { attributeBuffer in
                try vectors.withUnsafeBufferPointer { vectorBuffer in
                    try metadata.withUnsafeBufferPointer { metadataBuffer in
                        try texts.withUnsafeBufferPointer { textBuffer in
                            let records = documents.enumerated().map { index, document in
                                var embedded = ZeIngestDocument()
                                embedded.abi_size = abiSize(ZeIngestDocument.self)
                                embedded.doc_id = Self.cDocumentID(document.id)
                                embedded.revision = document.revision
                                embedded.timestamp = document.timestamp
                                embedded.vector = Self.pointer(
                                    vectorBuffer.baseAddress,
                                    offset: vectorOffsets[index],
                                    count: document.vector.count
                                )
                                embedded.vector_len = document.vector.count
                                embedded.metadata = Self.pointer(
                                    metadataBuffer.baseAddress,
                                    offset: metadataOffsets[index],
                                    count: document.metadata.count
                                )
                                embedded.metadata_len = document.metadata.count
                                embedded.text = Self.pointer(
                                    textBuffer.baseAddress,
                                    offset: textOffsets[index],
                                    count: textLengths[index]
                                )
                                embedded.text_len = textLengths[index]

                                var record = ZeUpsertDocument()
                                record.abi_size = abiSize(ZeUpsertDocument.self)
                                record.document = embedded
                                record.attributes = Self.pointer(
                                    attributeBuffer.baseAddress,
                                    offset: attributeOffsets[index],
                                    count: sortedAttributes[index].count
                                )
                                record.attribute_count = sortedAttributes[index].count
                                return record
                            }
                            return try records.withUnsafeBufferPointer { recordBuffer in
                                var request = ZeUpsertRequest()
                                request.abi_size = abiSize(ZeUpsertRequest.self)
                                request.documents = recordBuffer.baseAddress
                                request.document_count = records.count
                                request.dimension = documents.first?.vector.count ?? 0
                                var report = ZeMutationReport()
                                report.abi_size = abiSize(ZeMutationReport.self)
                                try checkZeppelin(ze_upsert(current, &request, &report))
                                return MutationReport(
                                    sequence: report.sequence,
                                    generation: report.generation
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}
