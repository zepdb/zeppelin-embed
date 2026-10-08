#if ZE_GRAPH
import CZeppelinEmbed
import Foundation

func graphSize<T>(_ type: T.Type) -> UInt32 { UInt32(MemoryLayout<T>.size) }
func graphArray<T>(_ pointer: UnsafePointer<T>?, _ count: Int) throws -> [T] {
  guard count >= 0, count <= 4 * 1024 * 1024 / max(1, MemoryLayout<T>.stride),
    count == 0 || pointer != nil
  else { throw GraphError(.invalidResponse("pointer/count")) }
  return Array(UnsafeBufferPointer(start: pointer, count: count))
}
func graphSlice<T>(_ array: [T], _ range: ZeGraphRange) throws -> [T] {
  let start = Int(range.start)
  let count = Int(range.count)
  guard start <= array.count, count <= array.count - start else {
    throw GraphError(.invalidResponse("range"))
  }
  return Array(array[start..<start + count])
}
func graphFlag(_ value: UInt32) throws -> Bool {
  guard value <= 1 else { throw GraphError(.invalidResponse("presence")) }
  return value == 1
}
struct GraphDecoder {
  let values: [ZeGraphValue], children: [UInt32], bytes: [UInt8], nodes: [ZeGraphNode],
    relationships: [ZeGraphRelationship], properties: [ZeGraphProperty], names: [ZeGraphRange],
    vectors: [Float]
  init(_ pool: ZeGraphValuePool) throws {
    values = try graphArray(pool.values, pool.value_count)
    children = try graphArray(pool.children, pool.child_count)
    bytes = try graphArray(pool.bytes, pool.byte_count)
    nodes = try graphArray(pool.nodes, pool.node_count)
    relationships = try graphArray(pool.relationships, pool.relationship_count)
    properties = try graphArray(pool.properties, pool.property_count)
    names = try graphArray(pool.names, pool.name_count)
    vectors = try graphArray(pool.vectors, pool.vector_count)
  }
  func string(_ range: ZeGraphRange) throws -> String {
    guard let text = String(bytes: try graphSlice(bytes, range), encoding: .utf8) else {
      throw GraphError(.invalidResponse("UTF-8"))
    }
    return text
  }
  func propertyMap(_ range: ZeGraphRange, depth: Int) throws -> [String: GraphValue] {
    var result: [String: GraphValue] = [:]
    for property in try graphSlice(properties, range) {
      let name = try string(property.name)
      guard result[name] == nil else { throw GraphError(.invalidResponse("duplicate property")) }
      result[name] = try value(property.value, depth: depth + 1)
    }
    return result
  }
  func node(_ index: UInt32, depth: Int = 0) throws -> GraphNode {
    guard Int(index) < nodes.count, depth <= 32 else {
      throw GraphError(.invalidResponse("node index/depth"))
    }
    let n = nodes[Int(index)]
    return try GraphNode(
      id: GraphNodeID(high: n.id.high, low: n.id.low),
      key: graphFlag(n.has_key)
        ? GraphKey(namespace: string(n.namespace_name), key: string(n.key)) : nil,
      revision: n.revision, lastChangeGeneration: n.last_change_generation,
      labels: graphSlice(names, n.labels).map(string),
      properties: propertyMap(n.properties, depth: depth),
      text: graphFlag(n.has_text) ? string(n.text) : nil,
      vector: graphFlag(n.has_vector) ? graphSlice(vectors, n.vector) : nil)
  }
  func relationship(_ index: UInt32, depth: Int) throws -> GraphRelationship {
    guard Int(index) < relationships.count, depth <= 32 else {
      throw GraphError(.invalidResponse("relationship index/depth"))
    }
    let r = relationships[Int(index)]
    return try GraphRelationship(
      id: GraphRelationshipID(high: r.id.high, low: r.id.low),
      source: GraphNodeID(high: r.source.high, low: r.source.low),
      target: GraphNodeID(high: r.target.high, low: r.target.low),
      key: graphFlag(r.has_key)
        ? GraphKey(namespace: string(r.namespace_name), key: string(r.key)) : nil,
      revision: r.revision, lastChangeGeneration: r.last_change_generation,
      type: string(r.relationship_type), properties: propertyMap(r.properties, depth: depth))
  }
  func value(_ index: UInt32, depth: Int = 0) throws -> GraphValue {
    guard Int(index) < values.count, depth <= 32 else {
      throw GraphError(.invalidResponse("value index/depth"))
    }
    let v = values[Int(index)]
    switch v.tag {
    case 0: return .null
    case 1: return .bool(try graphFlag(v.boolean))
    case 2: return .integer(v.integer)
    case 3: return .double(v.floating)
    case 4: return .string(try string(v.range))
    case 5: return .node(try node(v.entity_index, depth: depth + 1))
    case 6: return .relationship(try relationship(v.entity_index, depth: depth + 1))
    case 7:
      guard let kind = GraphListKind(rawValue: v.list_kind), kind != .empty || v.range.count == 0
      else { throw GraphError(.invalidResponse("list kind")) }
      let elements = try graphSlice(children, v.range).map { try value($0, depth: depth + 1) }
      for element in elements {
        switch (kind, element) {
        case (.query, _), (.bool, .bool), (.integer, .integer), (.double, .double),
          (.string, .string):
          break
        default: throw GraphError(.invalidResponse("list element type"))
        }
      }
      return .list(kind, elements)
    default: throw GraphError(.invalidResponse("value tag"))
    }
  }
}

// Copy metadata before checking status or decoding rows; the defer always frees
// the original native descriptor, never a modified decoder view.
func graphResponse(
  call: (inout ZeGraphResponse) throws -> Int32,
  free: (inout ZeGraphResponse) -> Int32 = { ze_graph_response_free(&$0) },
  nativeMessage: () -> String? = { nil }
) throws -> GraphResult {
  var response = ZeGraphResponse()
  response.abi_size = graphSize(ZeGraphResponse.self)
  defer { _ = free(&response) }
  let status = try call(&response)
  let message = status == 0 ? nil : nativeMessage()
  var metadata: GraphMetadata?
  do {
    guard let disposition = GraphDisposition(rawValue: response.disposition) else {
      throw GraphError(.invalidResponse("disposition"))
    }
    metadata = try GraphMetadata(
      disposition: disposition,
      admittedGeneration: graphFlag(response.has_admitted_generation)
        ? response.admitted_generation : nil,
      changedGeneration: graphFlag(response.has_changed_generation)
        ? response.changed_generation : nil, receipts: [], reports: [], diagnostics: [],
      globalWork: [])
    let decoder = try GraphDecoder(response.pool)
    let work = try graphArray(response.work, response.work_count).map {
      GraphWork(kind: $0.kind, units: $0.value)
    }
    let receipts = try graphArray(response.receipts, response.receipt_count).map {
      r -> GraphReceipt in
      guard let d = GraphDisposition(rawValue: r.disposition), r.entity_kind <= 1 else {
        throw GraphError(.invalidResponse("receipt"))
      }
      return try GraphReceipt(
        item: r.item,
        identity: r.entity_kind == 0
          ? .node(GraphNodeID(high: r.node.high, low: r.node.low))
          : .relationship(GraphRelationshipID(high: r.relationship.high, low: r.relationship.low)),
        disposition: d, deleted: graphFlag(r.deleted), revision: r.revision,
        generation: r.generation)
    }
    metadata?.receipts = receipts
    let diagnostics = try graphArray(response.diagnostics, response.diagnostic_count).map { d in
      try GraphDiagnostic(
        code: d.code,
        operatorIndex: graphFlag(d.operator_index.present) ? d.operator_index.index : nil,
        sourceSpan: graphFlag(d.has_source_span)
          ? Int(d.source_span.start)..<Int(d.source_span.start) + Int(d.source_span.count) : nil,
        message: decoder.string(d.message))
    }
    metadata?.diagnostics = diagnostics
    let reports = try graphArray(response.reports, response.report_count).map { r in
      try GraphSearchReport(
        call_id: r.call_id, kind: r.kind, generation: r.generation,
        has_requested_tier: r.has_requested_tier, requested_tier: r.requested_tier,
        has_actual_tier: r.has_actual_tier, actual_tier: r.actual_tier, precision: r.precision,
        coverage: r.coverage, vector_leg: r.vector_leg, lexical_leg: r.lexical_leg,
        has_document_epoch: r.has_document_epoch, has_query_epoch: r.has_query_epoch,
        has_tokenizer_epoch: r.has_tokenizer_epoch, cross_score_complete: r.cross_score_complete,
        document_epoch: r.document_epoch, query_epoch: r.query_epoch,
        tokenizer_epoch: r.tokenizer_epoch, effective_alpha: r.effective_alpha,
        normalization_version: r.normalization_version, rules_version: r.rules_version,
        candidate_count: r.candidate_count, cross_scored_count: r.cross_scored_count,
        fallback_count: r.fallback_count, work: graphSlice(work, r.work))
    }
    metadata = try GraphMetadata(
      disposition: disposition,
      admittedGeneration: graphFlag(response.has_admitted_generation)
        ? response.admitted_generation : nil,
      changedGeneration: graphFlag(response.has_changed_generation)
        ? response.changed_generation : nil, receipts: receipts, reports: reports,
      diagnostics: diagnostics, globalWork: graphSlice(work, response.global_work))
    if status != 0 {
      throw GraphError(.native(Int32(status)), metadata: metadata, nativeMessage: message)
    }
    guard response.row_count <= 65536, response.column_count <= 256,
      response.cell_count == response.row_count * response.column_count
    else { throw GraphError(.invalidResponse("row shape")) }
    let columns = try graphArray(response.columns, response.column_count).map {
      try GraphColumn(name: decoder.string($0.name), kinds: $0.kinds)
    }
    let cells = try graphArray(response.cells, response.cell_count).map { try decoder.value($0) }
    var rows: [[GraphValue]] = []
    for row in 0..<response.row_count {
      rows.append(Array(cells[row * columns.count..<(row + 1) * columns.count]))
    }
    guard let metadata else { throw GraphError(.invalidResponse("metadata")) }
    return GraphResult(metadata: metadata, columns: columns, rows: rows)
  } catch let error as GraphError {
    throw GraphError(
      error.reason, metadata: error.metadata ?? metadata,
      nativeMessage: error.nativeMessage ?? message)
  }
}

// Copy the native diagnostic immediately on the calling thread, before free.
func graphLastError(_ handle: UInt64) -> String? {
  var written = 0
  let probe = ze_last_error_message(handle, nil, 0, &written)
  guard probe == 0 else { return "native diagnostic unavailable (status \(probe))" }
  guard written > 0 else { return nil }
  guard written <= 4 * 1024 * 1024 else { return "native diagnostic exceeds 4 MiB" }
  var bytes = [CChar](repeating: 0, count: written + 1)
  let status = ze_last_error_message(handle, &bytes, bytes.count, &written)
  guard status == 0, written < bytes.count else {
    return "native diagnostic copy failed (status \(status))"
  }
  return String(bytes: bytes.prefix(written).map { UInt8(bitPattern: $0) }, encoding: .utf8)
    ?? "native diagnostic is not UTF-8"
}

#endif
