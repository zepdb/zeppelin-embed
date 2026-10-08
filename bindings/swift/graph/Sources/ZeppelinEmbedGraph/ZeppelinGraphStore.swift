import CZeppelinEmbedGraph
import Foundation

// Bridge the existing package's open options to the unified Store ABI.
func openGraphStoreHandle(_ request: UnsafePointer<ZeGraphOpenRequest>, _ handle: UnsafeMutablePointer<ze_handle>) -> Int32 {
  let request = request.pointee
  if request.mode == 0 { return withUnsafePointer(to: request) { ze_store_create_with_relationship_types($0, nil, 0, handle) } }
  guard request.mode <= 2 else { return Int32(ZE_ERR_INVALID_ARGUMENT.rawValue) }
  var open = ZeOpenRequest()
  open.abi_size = UInt32(MemoryLayout<ZeOpenRequest>.size)
  open.path = request.path.data; open.path_len = request.path.count
  open.access_mode = request.mode == 2 ? 1 : 0; open.durability_mode = 1; open.commit_tier = 2
  open.reader_drain_timeout_ms = request.reader_drain_timeout_ms
  open.max_resident_bytes = request.max_resident_bytes; open.max_temp_bytes = UInt64.max
  guard let tower = request.document_tower else { return ze_open(&open, handle) }
  var epoch = ZeEpochRequest()
  epoch.abi_size = UInt32(MemoryLayout<ZeEpochRequest>.size)
  epoch.embedding.document = tower.pointee; epoch.embedding.query = tower.pointee
  return ze_open_with_epoch(&open, &epoch, handle)
}

// Synchronous native seam used only by deterministic ownership/lifecycle tests.
struct GraphNativeCalls: Sendable {
  var apply:
    @Sendable (UInt64, UnsafePointer<ZeGraphBatchRequest>, UnsafeMutablePointer<ZeGraphResponse>) ->
      Int32 = { ze_store_graph_apply($0, $1, $2) }
  var query:
    @Sendable (UInt64, UnsafePointer<ZeGraphQueryRequest>, UnsafeMutablePointer<ZeGraphResponse>) ->
      Int32 = { ze_store_graph_query($0, $1, $2) }
  var getNodes:
    @Sendable (UInt64, UnsafePointer<ZeGraphGetNodesRequest>, UnsafeMutablePointer<ZeGraphResponse>)
      -> Int32 = { ze_store_get_nodes($0, $1, $2) }
  var getRelationships:
    @Sendable (UInt64, UnsafePointer<ZeGraphGetRelsRequest>, UnsafeMutablePointer<ZeGraphResponse>)
      -> Int32 = { ze_store_get_relationships($0, $1, $2) }
  var close: @Sendable (UInt64) -> Int32 = { ze_close($0) }
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

  // Native Cypher needs more stack than a Swift cooperative worker provides.
  // Keep each synchronous call and its borrowed buffers on one independent thread.
  nonisolated static func runBlocking<T: Sendable>(_ body: @escaping @Sendable () throws -> T)
    async throws -> T
  {
    try await withCheckedThrowingContinuation { continuation in
      let thread = Thread {
        continuation.resume(with: Result { try body() })
      }
      thread.stackSize = 8 * 1024 * 1024
      thread.start()
    }
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
          var handle = ze_handle(0)
          let status = openGraphStoreHandle(&request, &handle)
          guard status == 0 else { throw GraphError(.native(Int32(status))) }
          return handle
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
  public func resources() async throws -> GraphResources {
    let current = try openToken()
    return try await Self.runBlocking {
      var observation = ZeGraphResources()
      observation.abi_size = graphSize(ZeGraphResources.self)
      let status = ze_store_graph_resources(current, &observation)
      guard status == 0 else { throw GraphError(.native(Int32(status))) }
      return GraphResources(
        engineBytes: observation.engine_bytes, enginePeakBytes: observation.engine_peak_bytes,
        applicationBytes: observation.application_bytes,
        applicationPeakBytes: observation.application_peak_bytes)
    }
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
                call: { calls.apply(current, &request, &$0) }, free: calls.free,
                nativeMessage: { graphLastError(current) })
            }
          }
        }
      }
    }
  }
  public func cypher(
    _ query: String, parameters: [String: GraphParameter] = [:],
    controls: GraphControls = GraphControls(), options: GraphQueryOptions = GraphQueryOptions()
  ) async throws -> GraphResult {
    let current = try openToken()
    return try await Self.runBlocking {
      let encoder = GraphEncoder()
      let parameters = try encoder.parameters(parameters)
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
                return try options.withOptions { options in
                  request.options = options
                  if let limits = controls.compileLimits {
                    var limits = limits.native
                    return try withUnsafePointer(to: &limits) { limits in
                      request.compile_limits = limits
                      return try graphResponse(
                        call: {
                          ze_store_cypher_with_row_limit(
                            current, &request, controls.rowLimit, &$0)
                        }, nativeMessage: { graphLastError(current) })
                    }
                  }
                  return try graphResponse(
                    call: {
                      ze_store_cypher_with_row_limit(
                        current, &request, controls.rowLimit, &$0)
                    }, nativeMessage: { graphLastError(current) })
                }
              }
            }
          }
        }
      }
    }
  }
  public func query(
    _ plan: GraphPlan, parameters: [String: GraphParameter] = [:],
    options: GraphQueryOptions = GraphQueryOptions(), interruption: GraphInterruption = .none
  ) async throws -> GraphResult {
    let current = try openToken()
    let calls = self.calls
    return try await Self.runBlocking {
      let encoder = GraphEncoder()
      let bindings = try encoder.parameters(parameters)
      return try GraphPlanEncoder().withPlan(plan) { plan in
        try bindings.withUnsafeBufferPointer { bindings in
          try encoder.withPool { pool in
            var pool = pool
            return try withUnsafePointer(to: &pool) { pool in
              try options.withOptions { options in
                try GraphControls(interruption: interruption).withControl { control in
                  var request = ZeGraphQueryRequest()
                  request.abi_size = graphSize(ZeGraphQueryRequest.self)
                  request.plan = plan
                  request.parameters = bindings.baseAddress
                  request.parameter_count = bindings.count
                  request.parameter_pool = pool
                  request.options = options
                  request.control = control
                  return try graphResponse(
                    call: { calls.query(current, &request, &$0) }, free: calls.free,
                    nativeMessage: { graphLastError(current) })
                }
              }
            }
          }
        }
      }
    }
  }
  public func getNodes(
    _ ids: [GraphNodeID], fields: GraphNodeFields = GraphNodeFields(),
    limits: GraphQueryLimits? = nil, interruption: GraphInterruption = .none
  ) async throws -> GraphNodesResult {
    let current = try openToken()
    let calls = self.calls
    return try await Self.runBlocking {
      let nativeIDs = ids.map { ZeNodeId(high: $0.high, low: $0.low) }
      let result = try nativeIDs.withUnsafeBufferPointer { ids in
        try withLimits(limits) { limits in
          try GraphControls(interruption: interruption).withControl { control in
            var request = ZeGraphGetNodesRequest()
            request.abi_size = graphSize(ZeGraphGetNodesRequest.self)
            request.ids = ids.baseAddress
            request.id_count = ids.count
            request.include_text = fields.text ? 1 : 0
            request.include_vector = fields.vector ? 1 : 0
            request.limits = limits
            request.control = control
            return try graphResponse(
              call: { calls.getNodes(current, &request, &$0) }, free: calls.free,
              nativeMessage: { graphLastError(current) })
          }
        }
      }
      return try GraphNodesResult(result, count: ids.count)
    }
  }
  public func getRelationships(
    _ ids: [GraphRelationshipID], limits: GraphQueryLimits? = nil,
    interruption: GraphInterruption = .none
  ) async throws -> GraphRelationshipsResult {
    let current = try openToken()
    let calls = self.calls
    return try await Self.runBlocking {
      let nativeIDs = ids.map { ZeRelId(high: $0.high, low: $0.low) }
      let result = try nativeIDs.withUnsafeBufferPointer { ids in
        try withLimits(limits) { limits in
          try GraphControls(interruption: interruption).withControl { control in
            var request = ZeGraphGetRelsRequest()
            request.abi_size = graphSize(ZeGraphGetRelsRequest.self)
            request.ids = ids.baseAddress
            request.id_count = ids.count
            request.limits = limits
            request.control = control
            return try graphResponse(
              call: { calls.getRelationships(current, &request, &$0) }, free: calls.free,
              nativeMessage: { graphLastError(current) })
          }
        }
      }
      return try GraphRelationshipsResult(result, count: ids.count)
    }
  }
}
