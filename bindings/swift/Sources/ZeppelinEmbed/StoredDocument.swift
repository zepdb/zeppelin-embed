import CZeppelinEmbed
import Foundation

public struct StoredDocument: Sendable {
    public let id: DocumentID
    public let revision: UInt64
    public let timestamp: Int64
    public let vector: [Float]?
    public let text: String?
    public let metadata: Data?
    public let attributes: [UInt32: AttributeValue]
}

public struct DocumentFields: OptionSet, Sendable {
    public let rawValue: UInt32

    public init(rawValue: UInt32) {
        self.rawValue = rawValue
    }

    public static let vector = DocumentFields(rawValue: 1 << 0)
    public static let text = DocumentFields(rawValue: 1 << 1)
    public static let metadata = DocumentFields(rawValue: 1 << 2)
    public static let attributes = DocumentFields(rawValue: 1 << 3)
    public static let all: DocumentFields = [.vector, .text, .metadata, .attributes]
}

public struct GetResult: Sendable {
    public let documents: [StoredDocument?]
    public let missingCount: Int
    public let generation: UInt64
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public func get(
        _ ids: [DocumentID],
        fields: DocumentFields = .all
    ) async throws -> GetResult {
        let current = try openHandle()
        return try await Self.runBlocking {
            let rawIDs = ids.map(Self.cDocumentID)
            return try rawIDs.withUnsafeBufferPointer { idBuffer in
                var request = ZeGetRequest()
                request.abi_size = abiSize(ZeGetRequest.self)
                request.ids = idBuffer.baseAddress
                request.id_count = rawIDs.count
                request.include_vector = fields.contains(.vector) ? 1 : 0
                request.include_text = fields.contains(.text) ? 1 : 0
                request.include_metadata = fields.contains(.metadata) ? 1 : 0
                request.include_attributes = fields.contains(.attributes) ? 1 : 0
                var result = ZeGetResult()
                result.abi_size = abiSize(ZeGetResult.self)
                defer { _ = ze_get_result_free(&result) }
                try checkZeppelin(ze_get(current, &request, &result))
                return GetResult(
                    documents: try Self.copyStoredDocuments(
                        result.documents,
                        count: result.document_count,
                        fields: fields
                    ),
                    missingCount: result.missing_count,
                    generation: result.generation
                )
            }
        }
    }

    static func copyStoredDocuments(
        _ base: UnsafePointer<ZeStoredDocument>?,
        count: Int,
        fields: DocumentFields
    ) throws -> [StoredDocument?] {
        guard count > 0 else {
            return []
        }
        guard let base else {
            throw ZeppelinError.internalError
        }
        return try UnsafeBufferPointer(start: base, count: count).map { raw in
            guard raw.has_document == 1 else {
                return nil
            }
            let vector: [Float]?
            if fields.contains(.vector), raw.vector_len > 0 {
                guard let values = raw.vector else {
                    throw ZeppelinError.internalError
                }
                vector = Array(UnsafeBufferPointer(start: values, count: raw.vector_len))
            } else {
                vector = nil
            }

            let text: String?
            if fields.contains(.text), raw.text_len > 0 {
                guard let bytes = raw.text,
                    let decoded = String(
                        bytes: UnsafeBufferPointer(start: bytes, count: raw.text_len),
                        encoding: .utf8
                    )
                else {
                    throw ZeppelinError.internalError
                }
                text = decoded
            } else {
                text = nil
            }

            let metadata: Data?
            if fields.contains(.metadata) {
                if raw.metadata_len == 0 {
                    metadata = Data()
                } else if let bytes = raw.metadata {
                    metadata = Data(bytes: bytes, count: raw.metadata_len)
                } else {
                    throw ZeppelinError.internalError
                }
            } else {
                metadata = nil
            }

            var attributes: [UInt32: AttributeValue] = [:]
            if fields.contains(.attributes), raw.attribute_count > 0 {
                guard let values = raw.attributes else {
                    throw ZeppelinError.internalError
                }
                for value in UnsafeBufferPointer(start: values, count: raw.attribute_count) {
                    guard attributes[value.attribute_id] == nil else {
                        throw ZeppelinError.internalError
                    }
                    attributes[value.attribute_id] = try Self.attributeValue(value)
                }
            }
            return StoredDocument(
                id: DocumentID(high: raw.doc_id.high, low: raw.doc_id.low),
                revision: raw.revision,
                timestamp: raw.timestamp,
                vector: vector,
                text: text,
                metadata: metadata,
                attributes: attributes
            )
        }
    }

    private static func attributeValue(_ raw: ZeAttributeValue) throws -> AttributeValue {
        switch raw.value_type {
        case 0:
            return .null
        case 1:
            return .u64(raw.u64_value)
        case 2:
            return .i64(raw.i64_value)
        case 3:
            return .f64(raw.f64_value)
        case 4 where raw.bool_value == 0:
            return .bool(false)
        case 4 where raw.bool_value == 1:
            return .bool(true)
        case 5:
            guard raw.string_len == 0 || raw.string_value != nil,
                let value = String(
                    bytes: UnsafeBufferPointer(start: raw.string_value, count: raw.string_len),
                    encoding: .utf8
                )
            else {
                throw ZeppelinError.internalError
            }
            return .string(value)
        default:
            throw ZeppelinError.internalError
        }
    }
}
