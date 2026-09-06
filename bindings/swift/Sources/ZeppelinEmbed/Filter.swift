import CZeppelinEmbed
import Foundation

public struct RangeBound: Sendable, Equatable {
    public var value: AttributeValue
    public var inclusive: Bool

    public init(value: AttributeValue, inclusive: Bool) {
        self.value = value
        self.inclusive = inclusive
    }
}

public indirect enum Filter: Sendable {
    case eq(UInt32, AttributeValue)
    case notEq(UInt32, AttributeValue)
    case `in`(UInt32, [AttributeValue])
    case notIn(UInt32, [AttributeValue])
    case range(UInt32, lower: RangeBound?, upper: RangeBound?)
    case exists(UInt32)
    case isNull(UInt32)
    case and([Filter])
    case or([Filter])
    case not(Filter)
}

private struct FilterNodeRecord {
    var op: Int32 = 0
    var attributeID: UInt32 = 0
    var valueStart: Int = 0
    var valueCount: Int = 0
    var lowerIndex: Int?
    var lowerInclusive = false
    var upperIndex: Int?
    var upperInclusive = false
    var childrenStart: UInt32 = 0
    var childrenCount: UInt32 = 0
}

private struct FlattenedFilter {
    private static let maximumNodes = 1 << 20
    var nodes: [FilterNodeRecord] = []
    var values: [CAttributeValueRecord] = []
    var strings: [UInt8] = []

    init(_ filter: Filter) throws {
        _ = try append(filter, at: nil, depth: 1)
    }

    mutating func append(_ filter: Filter, at reserved: Int?, depth: Int) throws -> UInt32 {
        guard depth <= 32, nodes.count < Self.maximumNodes else {
            throw ZeppelinError.invalidArgument
        }
        let index: Int
        if let reserved {
            index = reserved
        } else {
            index = nodes.count
            nodes.append(FilterNodeRecord())
        }

        switch filter {
        case .eq(let id, let value):
            nodes[index] = valueNode(op: 1, id: id, values: [value])
        case .notEq(let id, let value):
            nodes[index] = valueNode(op: 2, id: id, values: [value])
        case .in(let id, let values):
            nodes[index] = valueNode(op: 3, id: id, values: values)
        case .notIn(let id, let values):
            nodes[index] = valueNode(op: 4, id: id, values: values)
        case .range(let id, let lower, let upper):
            var node = FilterNodeRecord(op: 5, attributeID: id)
            if let lower {
                node.lowerIndex = values.count
                node.lowerInclusive = lower.inclusive
                values.append(
                    CAttributeValueRecord(attributeID: id, value: lower.value, strings: &strings)
                )
            }
            if let upper {
                node.upperIndex = values.count
                node.upperInclusive = upper.inclusive
                values.append(
                    CAttributeValueRecord(attributeID: id, value: upper.value, strings: &strings)
                )
            }
            nodes[index] = node
        case .exists(let id):
            nodes[index] = FilterNodeRecord(op: 6, attributeID: id)
        case .isNull(let id):
            nodes[index] = FilterNodeRecord(op: 7, attributeID: id)
        case .and(let children):
            try appendLogical(op: 8, children: children, at: index, depth: depth)
        case .or(let children):
            try appendLogical(op: 9, children: children, at: index, depth: depth)
        case .not(let child):
            try appendLogical(op: 10, children: [child], at: index, depth: depth)
        }
        return UInt32(index)
    }

    mutating func valueNode(
        op: Int32,
        id: UInt32,
        values input: [AttributeValue]
    ) -> FilterNodeRecord {
        let start = values.count
        for value in input {
            values.append(CAttributeValueRecord(attributeID: id, value: value, strings: &strings))
        }
        return FilterNodeRecord(
            op: op,
            attributeID: id,
            valueStart: start,
            valueCount: input.count
        )
    }

    mutating func appendLogical(
        op: Int32,
        children: [Filter],
        at index: Int,
        depth: Int
    ) throws {
        guard nodes.count + children.count <= Self.maximumNodes else {
            throw ZeppelinError.invalidArgument
        }
        let start = nodes.count
        nodes.append(contentsOf: repeatElement(FilterNodeRecord(), count: children.count))
        nodes[index] = FilterNodeRecord(
            op: op,
            childrenStart: UInt32(start),
            childrenCount: UInt32(children.count)
        )
        for (offset, child) in children.enumerated() {
            _ = try append(child, at: start + offset, depth: depth + 1)
        }
    }

    func withUnsafeFilter<Result>(
        _ body: (UnsafePointer<ZeFilter>) throws -> Result
    ) rethrows -> Result {
        try strings.withUnsafeBufferPointer { stringBuffer in
            let rawValues = values.map { $0.rawValue(stringBase: stringBuffer.baseAddress) }
            return try rawValues.withUnsafeBufferPointer { valueBuffer in
                let rawNodes = nodes.map { record in
                    var node = ZeFilterNode()
                    node.op = record.op
                    node.attribute_id = record.attributeID
                    node.values = ZeppelinStore.pointer(
                        valueBuffer.baseAddress,
                        offset: record.valueStart,
                        count: record.valueCount
                    )
                    node.value_count = record.valueCount
                    if let lowerIndex = record.lowerIndex,
                        let lower = ZeppelinStore.pointer(
                            valueBuffer.baseAddress,
                            offset: lowerIndex,
                            count: 1
                        )
                    {
                        node.has_lower = 1
                        node.lower = lower.pointee
                        node.lower_inclusive = record.lowerInclusive ? 1 : 0
                    }
                    if let upperIndex = record.upperIndex,
                        let upper = ZeppelinStore.pointer(
                            valueBuffer.baseAddress,
                            offset: upperIndex,
                            count: 1
                        )
                    {
                        node.has_upper = 1
                        node.upper = upper.pointee
                        node.upper_inclusive = record.upperInclusive ? 1 : 0
                    }
                    node.children_start = record.childrenStart
                    node.children_count = record.childrenCount
                    return node
                }
                return try rawNodes.withUnsafeBufferPointer { nodeBuffer in
                    var filter = ZeFilter()
                    filter.abi_size = abiSize(ZeFilter.self)
                    filter.nodes = nodeBuffer.baseAddress
                    filter.node_count = rawNodes.count
                    filter.root = 0
                    return try withUnsafePointer(to: &filter, body)
                }
            }
        }
    }
}

func withUnsafeFilter<Result>(
    _ filter: Filter?,
    _ body: (UnsafePointer<ZeFilter>?) throws -> Result
) throws -> Result {
    guard let filter else {
        return try body(nil)
    }
    let flattened = try FlattenedFilter(filter)
    return try flattened.withUnsafeFilter { try body($0) }
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public func search(
        vector: [Float],
        filter: Filter,
        options: SearchOptions = SearchOptions()
    ) async throws -> SearchResult {
        let current = try openHandle()
        let cancellationToken = try options.cancellationToken?.rawValue() ?? 0
        return try await Self.runBlocking {
            try vector.withUnsafeBufferPointer { vectorBuffer in
                try withUnsafeFilter(filter) { filterPointer in
                    var search = ZeSearchRequest()
                    search.abi_size = abiSize(ZeSearchRequest.self)
                    search.vector = vectorBuffer.baseAddress
                    search.vector_len = vector.count
                    search.dimension = vector.count
                    search.k = options.k
                    search.thread_budget = options.threadBudget
                    search.has_tier = options.tier == nil ? 0 : 1
                    search.tier = options.tier?.rawValue ?? 0
                    search.graph_profile = options.graphProfile.rawValue
                    search.graph_ef = options.graphEF
                    search.graph_seed = options.graphSeed
                    search.cancel_token = cancellationToken
                    search.deadline_ns = options.deadlineNanoseconds

                    var request = ZeSearchFilteredRequest()
                    request.abi_size = abiSize(ZeSearchFilteredRequest.self)
                    request.search = search
                    request.filter = filterPointer
                    var result = ZeSearchResult()
                    result.abi_size = abiSize(ZeSearchResult.self)
                    defer { _ = ze_search_result_free(&result) }
                    try checkZeppelin(ze_search_filtered(current, &request, &result))
                    return SearchResult(
                        hits: try Self.copySearchHits(result),
                        generation: result.generation,
                        diagnostics: SearchDiagnostics(
                            dimensionsTouched: result.dims_touched,
                            bytesRead: result.bytes_read,
                            threadsUsed: result.threads_used,
                            graphSegmentsTraversed: result.graph_segments_traversed,
                            graphValidations: result.graph_validations,
                            graphEntrySeedDiscoveries: result.graph_entry_seed_discoveries,
                            graphVisitedEpochClears: result.graph_visited_epoch_clears,
                            graphCandidatesScored: result.graph_candidates_scored,
                            graphCandidatesRescored: result.graph_candidates_rescored,
                            graphSegmentsPrunedByBound: result.graph_segments_pruned_by_bound
                        )
                    )
                }
            }
        }
    }
}
