import Foundation

private struct JSONLineAttribute: Codable {
    var id: UInt32
    var type: String
    var u64: UInt64?
    var i64: Int64?
    var f64: Double?
    var bool: Bool?
    var string: String?

    init(id: UInt32, value: AttributeValue) {
        self.id = id
        switch value {
        case .null:
            type = "null"
        case .u64(let value):
            type = "u64"
            u64 = value
        case .i64(let value):
            type = "i64"
            i64 = value
        case .f64(let value):
            type = "f64"
            f64 = value
        case .bool(let value):
            type = "bool"
            bool = value
        case .string(let value):
            type = "string"
            string = value
        }
    }

    func attributeValue() throws -> AttributeValue {
        switch type {
        case "null":
            return .null
        case "u64" where u64 != nil:
            return .u64(u64 ?? 0)
        case "i64" where i64 != nil:
            return .i64(i64 ?? 0)
        case "f64" where f64 != nil:
            return .f64(f64 ?? 0)
        case "bool" where bool != nil:
            return .bool(bool ?? false)
        case "string" where string != nil:
            return .string(string ?? "")
        default:
            throw DecodingError.dataCorrupted(
                DecodingError.Context(
                    codingPath: [],
                    debugDescription: "invalid JSONL attribute payload for id \(id)"
                )
            )
        }
    }
}

private struct JSONLineDocument: Codable {
    var idHigh: UInt64
    var idLow: UInt64
    var revision: UInt64
    var timestamp: Int64
    var vector: [Float]?
    var text: String?
    var metadata: Data?
    var attributes: [JSONLineAttribute]

    private enum CodingKeys: String, CodingKey {
        case idHigh = "id_high"
        case idLow = "id_low"
        case revision
        case timestamp
        case vector
        case text
        case metadata
        case attributes
    }

    init(_ document: StoredDocument) {
        idHigh = document.id.high
        idLow = document.id.low
        revision = document.revision
        timestamp = document.timestamp
        vector = document.vector
        text = document.text
        metadata = document.metadata
        attributes = document.attributes.sorted { $0.key < $1.key }.map {
            JSONLineAttribute(id: $0.key, value: $0.value)
        }
    }

    func ingestDocument() throws -> IngestDocument {
        var values: [UInt32: AttributeValue] = [:]
        for attribute in attributes {
            guard values[attribute.id] == nil else {
                throw DecodingError.dataCorrupted(
                    DecodingError.Context(
                        codingPath: [],
                        debugDescription: "duplicate JSONL attribute id \(attribute.id)"
                    )
                )
            }
            values[attribute.id] = try attribute.attributeValue()
        }
        return IngestDocument(
            id: DocumentID(high: idHigh, low: idLow),
            revision: revision,
            timestamp: timestamp,
            vector: vector ?? [],
            text: text,
            metadata: metadata ?? Data(),
            attributes: values
        )
    }
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public func exportJSONLines(to url: URL, fields: DocumentFields) async throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        var output = Data()
        var cursor: ScanCursor?
        repeat {
            let page = try await scan(after: cursor, fields: fields)
            for document in page.documents {
                output.append(try encoder.encode(JSONLineDocument(document)))
                output.append(0x0A)
            }
            cursor = page.next
        } while cursor != nil
        try output.write(to: url, options: .atomic)
    }

    public func importJSONLines(from url: URL) async throws {
        let input = try Data(contentsOf: url)
        let decoder = JSONDecoder()
        var batch: [IngestDocument] = []
        batch.reserveCapacity(1_024)
        for line in input.split(separator: 0x0A) {
            let decoded = try decoder.decode(JSONLineDocument.self, from: Data(line))
            batch.append(try decoded.ingestDocument())
            if batch.count == 1_024 {
                _ = try await upsert(batch)
                batch.removeAll(keepingCapacity: true)
            }
        }
        if !batch.isEmpty {
            _ = try await upsert(batch)
        }
    }
}
