import CZeppelinEmbedGraph
import Foundation

// Synchronous native seam used only by deterministic ownership/lifecycle tests.
struct GraphNativeCalls: Sendable {
  var apply:
    @Sendable (UInt64, UnsafePointer<ZeGraphBatchRequest>, UnsafeMutablePointer<ZeGraphResponse>) ->
      Int32 = { ze_graph_apply(ZeGraphHandle(token: $0), $1, $2) }
  var close: @Sendable (UInt64) -> Int32 = { ze_graph_close(ZeGraphHandle(token: $0)) }
  var free: @Sendable (inout ZeGraphResponse) -> Int32 = { ze_graph_response_free(&$0) }
}

@available(macOS 14.0, *)
public actor ZeppelinGraphStore {
  public enum OpenMode: UInt32, Sendable { case create, readWrite, readOnly }
  private var token: UInt64?
  private var closing = false
  private let calls: GraphNativeCalls
  init(token: UInt64, calls: GraphNativeCalls = GraphNativeCalls()) {
    self.token = token
    self.calls = calls
  }
  deinit { if let token { _ = calls.close(token) } }

  // Same detached blocking-call mechanism as ZeppelinStore, in the separate
  // graph product. No actor write queue, automatic retries or Task bridge.
  nonisolated static func runBlocking<T: Sendable>(_ body: @escaping @Sendable () throws -> T)
    async throws -> T
  {
    try await Task.detached(operation: body).value
  }
  public static func open(
    at path: URL, mode: OpenMode = .readWrite,
    documentTower: EmbeddingTower? = nil,
    maxResidentBytes: UInt64 = 256 * 1024 * 1024,
    readerDrainTimeoutMilliseconds: UInt64 = 5000
  ) async throws -> ZeppelinGraphStore {
    let token = try await runBlocking {
      let bytes = Array(path.path.utf8)
      return try bytes.withUnsafeBufferPointer { bytes in
        return try withGraphTower(documentTower) { tower in
          var request = ZeGraphOpenRequest()
          request.abi_size = graphSize(ZeGraphOpenRequest.self)
          request.path = ZeGraphBytes(data: bytes.baseAddress, count: bytes.count)
          request.mode = mode.rawValue
          request.max_resident_bytes = maxResidentBytes
          request.reader_drain_timeout_ms = readerDrainTimeoutMilliseconds
          request.control = nil
          request.document_tower = tower
          var handle = ZeGraphHandle()
          let status = ze_graph_open(&request, &handle)
          guard status == 0 else { throw GraphError(.native(Int32(status))) }
          return handle.token
        }
      }
    }
    return ZeppelinGraphStore(token: token)
  }
  private func openToken() throws -> UInt64 {
    guard !closing else { throw GraphError(.native(ZeppelinError.closing.rawValue)) }
    guard let token else { throw GraphError(.closed) }
    return token
  }
  public func close() async throws {
    guard let current = token else { return }
    guard !closing else { throw GraphError(.native(ZeppelinError.closing.rawValue)) }
    closing = true
    // graph_abi::close releases the slot even if checkpoint/drain returns an error.
    defer {
      token = nil
      closing = false
    }
    let close = calls.close
    let status = await Task.detached { close(current) }.value
    // Preserve native close errors; never resurrect the consumed handle.
    guard status == 0 else { throw GraphError(.native(Int32(status))) }

  }
  public func apply(_ batch: GraphBatch, interruption: GraphInterruption = .none) async throws
    -> GraphResult
  {
    let current = try openToken()
    let calls = self.calls
    return try await Self.runBlocking {
      let encoder = GraphEncoder()
      let items = try encoder.batch(batch)
      return try items.withUnsafeBufferPointer { items in
        try encoder.withPool { pool in
          var pool = pool
          return try withUnsafePointer(to: &pool) { pool in
            try GraphControls(interruption: interruption).withControl { control in
              var request = ZeGraphBatchRequest()
              request.abi_size = graphSize(ZeGraphBatchRequest.self)
              request.items = items.baseAddress
              request.item_count = items.count
              request.pool = pool
              request.control = control
              return try graphResponse(
                call: { calls.apply(current, &request, &$0) }, free: calls.free)
            }
          }
        }
      }
    }
  }
  public func cypher(
    _ query: String, parameters: [String: GraphParameter] = [:],
    controls: GraphControls = GraphControls()
  ) async throws -> GraphResult {
    let current = try openToken()
    return try await Self.runBlocking {
      let encoder = GraphEncoder()
      let parameters = try parameters.keys.sorted().map { name -> ZeGraphParameterValue in
        guard let value = parameters[name] else { throw GraphError(.invalidRequest("parameter")) }
        var p = ZeGraphParameterValue()
        p.abi_size = graphSize(ZeGraphParameterValue.self)
        p.name = try encoder.string(name)
        p.value = try encoder.scalar(value)
        return p
      }
      let bytes = Array(query.utf8)
      return try bytes.withUnsafeBufferPointer { bytes in
        try parameters.withUnsafeBufferPointer { parameters in
          try encoder.withPool { pool in
            var pool = pool
            return try withUnsafePointer(to: &pool) { pool in
              try controls.withControl { control in
                var request = ZeGraphCypherRequest()
                request.abi_size = graphSize(ZeGraphCypherRequest.self)
                request.query = ZeGraphBytes(data: bytes.baseAddress, count: bytes.count)
                request.parameters = parameters.baseAddress
                request.parameter_count = parameters.count
                request.parameter_pool = pool
                request.control = control
                // HEAD rejects query options; expose only working row/compiler bounds.
                if let limits = controls.compileLimits {
                  var limits = limits.native
                  return try withUnsafePointer(to: &limits) { limits in
                    request.compile_limits = limits
                    return try graphResponse {
                      ze_graph_cypher_with_row_limit(
                        ZeGraphHandle(token: current), &request, controls.rowLimit, &$0)
                    }
                  }
                }
                return try graphResponse {
                  ze_graph_cypher_with_row_limit(
                    ZeGraphHandle(token: current), &request, controls.rowLimit, &$0)
                }
              }
            }
          }
        }
      }
    }
  }
  // ZE-241's follow-up will attach structured query/getNodes/getRelationships
  // here, using the same owned GraphResult/GraphError and response boundary.
}
