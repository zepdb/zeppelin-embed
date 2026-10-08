#if ZE_GRAPH
import CZeppelinEmbed
import Foundation

// Synchronous native seam used only by deterministic ownership/lifecycle tests.
struct GraphNativeCalls: Sendable {
  var apply:
    @Sendable (UInt64, UnsafePointer<ZeGraphBatchRequest>, UnsafeMutablePointer<ZeGraphResponse>) ->
      Int32 = { ze_store_graph_apply($0, $1, $2) }
  var applyV2:
    @Sendable (UInt64, UnsafePointer<ZeStoreGraphBatchRequestV2>, UnsafeMutablePointer<ZeGraphResponse>) -> Int32 = { ze_store_graph_apply_v2($0, $1, $2) }
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

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
  // Native Cypher needs more stack than a Swift cooperative worker provides.
  // Keep each synchronous call and its borrowed buffers on one independent thread.
  nonisolated static func runGraphBlocking<T: Sendable>(_ body: @escaping @Sendable () throws -> T)
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
  public func enableGraph() async throws -> UInt64 {
    let current = try openHandle()
    return try await Self.runGraphBlocking {
      var report = ZeGenerationReport()
      report.abi_size = graphSize(ZeGenerationReport.self)
      try checkZeppelin(ze_store_enable_graph(current, &report))
      return report.generation
    }
  }
  public func graphResources() async throws -> GraphResources {
    let current = try openHandle()
    return try await Self.runGraphBlocking {
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
  public func graphApply(_ batch: GraphBatch, interruption: GraphInterruption = .none) async throws
    -> GraphResult
  {
    let current = try openHandle()
    let calls = self.graphCalls
    return try await Self.runGraphBlocking {
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
                call: { response in
                  try encoder.withDocuments(batch) { documents in
                    if documents.isEmpty { return calls.apply(current, &request, &response) }
                    return withUnsafePointer(to: &request) { graph in
                      var v2 = ZeStoreGraphBatchRequestV2()
                      v2.abi_size = graphSize(ZeStoreGraphBatchRequestV2.self)
                      v2.graph = graph
                      v2.documents = documents.baseAddress
                      v2.document_count = documents.count
                      return calls.applyV2(current, &v2, &response)
                    }
                  }
                }, free: calls.free,
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
    let current = try openHandle()
    return try await Self.runGraphBlocking {
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
  public func graphQuery(
    _ plan: GraphPlan, parameters: [String: GraphParameter] = [:],
    options: GraphQueryOptions = GraphQueryOptions(), interruption: GraphInterruption = .none
  ) async throws -> GraphResult {
    let current = try openHandle()
    let calls = self.graphCalls
    return try await Self.runGraphBlocking {
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
    let current = try openHandle()
    let calls = self.graphCalls
    return try await Self.runGraphBlocking {
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
    let current = try openHandle()
    let calls = self.graphCalls
    return try await Self.runGraphBlocking {
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

#endif
