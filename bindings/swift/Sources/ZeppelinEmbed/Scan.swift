import CZeppelinEmbed
import Foundation

public enum ScanOrder: Int32, Sendable {
    case storage = 0
    case timestampAscending = 1
    case timestampDescending = 2
}

public struct ScanCursor: Sendable {
    let generation: UInt64
    let segmentID: [UInt8]
    let row: UInt32
    let phase: UInt32
}

public struct ScanPage: Sendable {
    public let documents: [StoredDocument]
    public let generation: UInt64
    public let next: ScanCursor?
}

public struct CountResult: Sendable {
    public let count: UInt64
    public let generation: UInt64
}

private actor DocumentStreamPager {
    let store: ZeppelinStore
    let order: ScanOrder
    let fields: DocumentFields
    let filter: Filter?
    var cursor: ScanCursor?
    var documents: [StoredDocument] = []
    var position = 0
    var finished = false

    init(store: ZeppelinStore, order: ScanOrder, fields: DocumentFields, filter: Filter?) {
        self.store = store
        self.order = order
        self.fields = fields
        self.filter = filter
    }

    func next() async throws -> StoredDocument? {
        while position == documents.count {
            guard !finished else {
                return nil
            }
            let page = try await store.scan(
                after: cursor,
                limit: 1_024,
                order: order,
                fields: fields,
                filter: filter
            )
            documents = page.documents
            position = 0
            cursor = page.next
            finished = page.next == nil
        }
        defer { position += 1 }
        return documents[position]
    }
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public func scan(
        after cursor: ScanCursor? = nil,
        limit: Int = 1_024,
        order: ScanOrder = .storage,
        fields: DocumentFields = .all,
        timestampRange: Range<Int64>? = nil,
        filter: Filter? = nil,
        cancellationToken: ZeppelinCancellationToken? = nil,
        deadlineNanoseconds: UInt64 = 0
    ) async throws -> ScanPage {
        let current = try openHandle()
        let token = try cancellationToken?.rawValue() ?? 0
        return try await Self.runBlocking {
            try withUnsafeFilter(filter) { filterPointer in
                var request = ZeScanRequest()
                request.abi_size = abiSize(ZeScanRequest.self)
                if let cursor {
                    request.cursor_generation = cursor.generation
                    withUnsafeMutableBytes(of: &request.cursor_segment_id) { bytes in
                        bytes.copyBytes(from: cursor.segmentID)
                    }
                    request.cursor_next_row = cursor.row
                    request.cursor_phase = cursor.phase
                }
                request.limit = limit
                request.order = order.rawValue
                request.include_vector = fields.contains(.vector) ? 1 : 0
                request.include_text = fields.contains(.text) ? 1 : 0
                request.include_metadata = fields.contains(.metadata) ? 1 : 0
                request.include_attributes = fields.contains(.attributes) ? 1 : 0
                if let timestampRange {
                    request.has_timestamp_range = 1
                    request.start_ts = timestampRange.lowerBound
                    request.end_ts = timestampRange.upperBound
                }
                request.filter = filterPointer
                request.cancel_token = token
                request.deadline_ns = deadlineNanoseconds

                var result = ZeScanResult()
                result.abi_size = abiSize(ZeScanResult.self)
                defer { _ = ze_scan_result_free(&result) }
                try checkZeppelin(ze_scan(current, &request, &result))
                let copied = try Self.copyStoredDocuments(
                    result.documents,
                    count: result.document_count,
                    fields: fields
                ).map { document -> StoredDocument in
                    guard let document else {
                        throw ZeppelinError.internalError
                    }
                    return document
                }
                let next: ScanCursor?
                switch result.has_more {
                case 0:
                    next = nil
                case 1:
                    var segmentID = result.next_segment_id
                    let bytes = withUnsafeBytes(of: &segmentID) { Array($0.prefix(16)) }
                    next = ScanCursor(
                        generation: result.generation,
                        segmentID: bytes,
                        row: result.next_row,
                        phase: result.next_phase
                    )
                default:
                    throw ZeppelinError.internalError
                }
                return ScanPage(documents: copied, generation: result.generation, next: next)
            }
        }
    }

    public nonisolated func documents(
        order: ScanOrder = .storage,
        fields: DocumentFields = .all,
        filter: Filter? = nil
    ) -> AsyncThrowingStream<StoredDocument, Error> {
        let pager = DocumentStreamPager(store: self, order: order, fields: fields, filter: filter)
        return AsyncThrowingStream(unfolding: {
            try await pager.next()
        })
    }

    public func count(
        filter: Filter? = nil,
        timestampRange: Range<Int64>? = nil
    ) async throws -> CountResult {
        let current = try openHandle()
        return try await Self.runBlocking {
            try withUnsafeFilter(filter) { filterPointer in
                var request = ZeCountRequest()
                request.abi_size = abiSize(ZeCountRequest.self)
                request.filter = filterPointer
                if let timestampRange {
                    request.has_timestamp_range = 1
                    request.start_ts = timestampRange.lowerBound
                    request.end_ts = timestampRange.upperBound
                }
                var result = ZeCountResult()
                result.abi_size = abiSize(ZeCountResult.self)
                try checkZeppelin(ze_count(current, &request, &result))
                return CountResult(count: result.count, generation: result.generation)
            }
        }
    }
}
