#if ZE_GRAPH
import CZeppelinEmbed
import XCTest

@testable import ZeppelinEmbed

private enum ReadEntry: CaseIterable, Sendable {
  case query, nodes, relationships
  func invoke(_ store: ZeppelinStore, interruption: GraphInterruption = .none) async throws
    -> GraphMetadata
  {
    switch self {
    case .query:
      return try await store.graphQuery(
        GraphPlan(root: GraphOperatorID(0), operators: [.unit]), interruption: interruption
      ).metadata
    case .nodes: return try await store.getNodes([], interruption: interruption).metadata
    case .relationships:
      return try await store.getRelationships([], interruption: interruption).metadata
    }
  }
}
private final class ReadProbe: @unchecked Sendable {
  let column = UnsafeMutablePointer<ZeGraphColumn>.allocate(capacity: 1)
  let entered = DispatchSemaphore(value: 0), finish = DispatchSemaphore(value: 0)
  private let lock = NSLock()
  private var freeCount = 0
  init() { column.initialize(to: ZeGraphColumn()) }
  deinit {
    column.deinitialize(count: 1)
    column.deallocate()
  }
  func freed() {
    lock.lock()
    freeCount += 1
    lock.unlock()
  }
  var frees: Int {
    lock.lock()
    defer { lock.unlock() }
    return freeCount
  }
  func pause() {
    entered.signal()
    XCTAssertEqual(finish.wait(timeout: .now() + 10), .success)
  }
  private func waitSynchronously() -> Bool { entered.wait(timeout: .now() + 10) == .success }
  func wait() async -> Bool { await Task.detached { self.waitSynchronously() }.value }
  func response(_ response: UnsafeMutablePointer<ZeGraphResponse>, getter: Bool) {
    response.pointee.has_admitted_generation = 1
    response.pointee.admitted_generation = 42
    if getter {
      response.pointee.column_count = 1
      response.pointee.columns = UnsafePointer(column)
    }
  }
}

final class GraphReadBoundaryTests: XCTestCase {
  func testEveryReadEntryFreesExactlyOnceOnSuccessNativeErrorAndDecodeFailure() async throws {
    for entry in ReadEntry.allCases {
      for mode in 0..<3 {
        let probe = ReadProbe()
        let complete: @Sendable (UnsafeMutablePointer<ZeGraphResponse>) -> Int32 = { response in
          probe.response(response, getter: entry != .query)
          if mode == 1 {
            response.pointee.disposition = 2
            response.pointee.has_changed_generation = 1
            response.pointee.changed_generation = 91
            return 9
          }
          if mode == 2 {
            response.pointee.pool.values = nil
            response.pointee.pool.value_count = 1
          }
          return 0
        }
        let calls = GraphNativeCalls(
          query: { _, _, r in complete(r) }, getNodes: { _, _, r in complete(r) },
          getRelationships: { _, _, r in complete(r) }, close: { _ in 0 },
          free: { _ in
            probe.freed()
            return 0
          })
        let store = makeGraphTestStore(handle: 1, calls: calls)
        do {
          let metadata = try await entry.invoke(store)
          XCTAssertEqual(mode, 0)
          XCTAssertEqual(metadata.admittedGeneration, 42)
        } catch let error as GraphError {
          XCTAssertNotEqual(mode, 0)
          XCTAssertEqual(error.metadata?.admittedGeneration, 42)
          if mode == 1 {
            XCTAssertEqual(error.code, .io)
            XCTAssertEqual(error.metadata?.changedGeneration, 91)
            XCTAssertEqual(error.metadata?.disposition, .committed)
          } else {
            guard case .invalidResponse = error.reason else { return XCTFail("decode failure") }
          }
        }
        XCTAssertEqual(probe.frees, 1)
        try await store.close()
      }
    }
  }
  func testGettersRejectWrongEntityAndShapeAfterFree() async throws {
    for nodes in [true, false] {
      let probe = ReadProbe()
      let complete: @Sendable (UnsafeMutablePointer<ZeGraphResponse>) -> Int32 = { response in
        probe.response(response, getter: false)  // zero columns violates getter contract
        return 0
      }
      let store = makeGraphTestStore(
        handle: 1,
        calls: GraphNativeCalls(
          getNodes: { _, _, r in complete(r) }, getRelationships: { _, _, r in complete(r) },
          close: { _ in 0 },
          free: { _ in
            probe.freed()
            return 0
          }))
      do {
        if nodes {
          _ = try await store.getNodes([])
        } else {
          _ = try await store.getRelationships([])
        }
        XCTFail("getter shape")
      } catch let error as GraphError {
        guard case .invalidResponse = error.reason else { return XCTFail("shape error") }
        XCTAssertEqual(error.metadata?.admittedGeneration, 42)
      }
      XCTAssertEqual(probe.frees, 1)
      try await store.close()
    }
    let metadata = GraphMetadata(
      disposition: .notApplicable, admittedGeneration: 42, changedGeneration: nil,
      receipts: [], reports: [], diagnostics: [], globalWork: [])
    let wrongValue = GraphResult(
      metadata: metadata, columns: [GraphColumn(name: "entity", kinds: 4)], rows: [[.integer(1)]])
    XCTAssertThrowsError(try GraphNodesResult(wrongValue, count: 1))
    XCTAssertThrowsError(try GraphRelationshipsResult(wrongValue, count: 1))
  }
  func testFullWidthIDsAndTypedOptionsAreBorrowedWithoutNarrowing() async throws {
    let probe = ReadProbe()
    let high = UInt64.max
    let low: UInt64 = 0x8000_0000_0000_0001
    let calls = GraphNativeCalls(
      query: { _, request, _ in
        let plan = request.pointee.plan!.pointee
        let ops = Array(UnsafeBufferPointer(start: plan.operators, count: plan.operator_count))
        XCTAssertEqual(ops[0].node_id.high, high)
        XCTAssertEqual(ops[0].node_id.low, low)
        XCTAssertEqual(ops[1].relationship_id.high, low)
        XCTAssertEqual(ops[1].relationship_id.low, high)
        XCTAssertEqual(ops[2].inputs.count, 2)
        let options = request.pointee.options!.pointee
        XCTAssertEqual(options.limits!.pointee.query_bytes, 1024)
        XCTAssertEqual(options.limits!.pointee.work_count, 2)
        let work = Array(UnsafeBufferPointer(start: options.limits!.pointee.work, count: 2))
        XCTAssertEqual(work.map(\.kind), [2, 13])
        XCTAssertEqual(work.map(\.limit), [0, 3])
        XCTAssertEqual(request.pointee.control!.pointee.deadline_ns, 55)
        return 7
      },
      getNodes: { _, request, _ in
        let ids = Array(
          UnsafeBufferPointer(start: request.pointee.ids, count: request.pointee.id_count))
        XCTAssertEqual(ids.map(\.high), [high, low])
        XCTAssertEqual(ids.map(\.low), [low, high])
        XCTAssertEqual(request.pointee.include_text, 1)
        XCTAssertEqual(request.pointee.include_vector, 1)
        XCTAssertEqual(request.pointee.limits!.pointee.has_query_bytes, 1)
        return 7
      },
      getRelationships: { _, request, _ in
        let ids = Array(
          UnsafeBufferPointer(start: request.pointee.ids, count: request.pointee.id_count))
        XCTAssertEqual(ids.map(\.high), [low, high])
        XCTAssertEqual(ids.map(\.low), [high, low])
        return 7
      }, close: { _ in 0 },
      free: { _ in
        probe.freed()
        return 0
      })
    let store = makeGraphTestStore(handle: 1, calls: calls)
    let plan = GraphPlan(
      root: GraphOperatorID(2),
      operators: [
        .lookupNode(output: GraphSlotID(0), id: GraphNodeID(high: high, low: low)),
        .lookupRelationship(output: GraphSlotID(1), id: GraphRelationshipID(high: low, low: high)),
        .join(left: GraphOperatorID(0), right: GraphOperatorID(1), predicate: nil),
      ])
    let limits = GraphQueryLimits(queryBytes: 1024, work: [.lookups: 3, .expressions: 0])
    do {
      _ = try await store.graphQuery(
        plan, options: GraphQueryOptions(limits: limits), interruption: .deadlineNanoseconds(55))
      XCTFail("busy")
    } catch let e as GraphError { XCTAssertEqual(e.code, .busy) }
    do {
      _ = try await store.getNodes(
        [GraphNodeID(high: high, low: low), GraphNodeID(high: low, low: high)],
        fields: GraphNodeFields(text: true, vector: true), limits: limits)
      XCTFail("busy")
    } catch let e as GraphError { XCTAssertEqual(e.code, .busy) }
    do {
      _ = try await store.getRelationships([
        GraphRelationshipID(high: low, low: high), GraphRelationshipID(high: high, low: low),
      ])
      XCTFail("busy")
    } catch let e as GraphError { XCTAssertEqual(e.code, .busy) }
    XCTAssertEqual(probe.frees, 3)
    try await store.close()
  }
  func testDocumentAndGraphOperationsShareCloseState() async throws {
    for entry in ReadEntry.allCases {
      for status: Int32 in [0, 9] {
        let read = ReadProbe()
        let close = ReadProbe()
        let cancellation = try GraphCancellationToken()
        let complete:
          @Sendable (UnsafePointer<ZeGraphControl>?, UnsafeMutablePointer<ZeGraphResponse>) -> Int32 =
            { control, response in
              XCTAssertEqual(control?.pointee.cancel_token, cancellation.value)
              read.pause()
              read.response(response, getter: entry != .query)
              return status
            }
        let store = makeGraphTestStore(
          handle: 1,
          calls: GraphNativeCalls(
            query: { _, request, r in complete(request.pointee.control, r) },
            getNodes: { _, request, r in complete(request.pointee.control, r) },
            getRelationships: { _, request, r in complete(request.pointee.control, r) },
            close: { _ in
              close.pause()
              return 0
            },
            free: { _ in
              read.freed()
              return 0
            }))
        let task = Task { try await entry.invoke(store, interruption: .cancellation(cancellation)) }
        let readEntered = await read.wait()
        XCTAssertTrue(readEntered)
        task.cancel()
        try cancellation.cancel()
        let closing = Task { try await store.close() }
        let closeEntered = await close.wait()
        XCTAssertTrue(closeEntered)
        do { _ = try await store.state(); XCTFail("document admission after close") }
        catch let error as ZeppelinError { XCTAssertEqual(error, .closed) }
        do {
          _ = try await entry.invoke(store)
          XCTFail("closing admission")
        } catch let e as ZeppelinError { XCTAssertEqual(e, .closed) }
        read.finish.signal()
        do {
          let metadata = try await task.value
          XCTAssertEqual(status, 0)
          XCTAssertEqual(metadata.admittedGeneration, 42)
        } catch let e as GraphError {
          XCTAssertEqual(status, 9)
          XCTAssertEqual(e.code, .io)
          XCTAssertEqual(e.metadata?.admittedGeneration, 42)
        }
        XCTAssertEqual(read.frees, 1)
        close.finish.signal()
        try await closing.value
        do {
          _ = try await entry.invoke(store)
          XCTFail("closed admission")
        } catch let e as ZeppelinError { XCTAssertEqual(e, .closed) }
      }
    }
  }
}

extension GraphReadBoundaryTests {
  func testStructuredNamedListBindingsAndSearchOptionsReachCUnchanged() async throws {
    let probe = ReadProbe()
    let calls = GraphNativeCalls(
      query: { _, request, response in
        let request = request.pointee
        let plan = request.plan!.pointee
        let declarations = Array(
          UnsafeBufferPointer(start: plan.parameters, count: plan.parameter_count))
        XCTAssertEqual(declarations.count, 2)
        XCTAssertEqual(declarations.map(\.kinds), [128, 16])
        let decoder = try! GraphDecoder(request.parameter_pool!.pointee)
        let bindings = Array(
          UnsafeBufferPointer(start: request.parameters, count: request.parameter_count))
        XCTAssertEqual(try! bindings.map { try decoder.string($0.name) }, ["text", "vector"])
        XCTAssertEqual(try! decoder.value(bindings[0].value), .string("amber"))
        XCTAssertEqual(
          try! decoder.value(bindings[1].value), .list(.query, [.double(1), .double(0)]))
        XCTAssertEqual(plan.eager_search_count, 1)
        XCTAssertEqual(plan.eager_searches?.pointee, 0)
        let search = plan.searches!.pointee
        XCTAssertEqual(search.kind, 2)
        XCTAssertEqual(search.has_tier, 1)
        XCTAssertEqual(search.tier, 0)
        XCTAssertEqual(search.vector.index, 0)
        XCTAssertEqual(search.text.index, 1)
        XCTAssertEqual(search.vector_distance_slot.present, 1)
        XCTAssertEqual(search.lexical_score_slot.index, 3)
        let options = search.options!.pointee
        XCTAssertEqual(options.graph_profile, 1)
        XCTAssertEqual(options.graph_ef, 12)
        XCTAssertEqual(options.graph_seed, 99)
        XCTAssertEqual(options.lexical_flags, 1)
        XCTAssertEqual(options.rescore, 1)
        XCTAssertEqual(options.has_alpha, 1)
        XCTAssertEqual(options.alpha, 0.25)
        XCTAssertEqual(options.rules_enabled, 1)
        XCTAssertEqual(options.has_max_rounds, 1)
        XCTAssertEqual(options.max_rounds, 0)
        probe.response(response, getter: false)
        return 0
      }, close: { _ in 0 },
      free: { _ in
        probe.freed()
        return 0
      })
    let store = makeGraphTestStore(handle: 1, calls: calls)
    var options = GraphSearchOptions()
    options.profile = .angular
    options.ef = 12
    options.seed = 99
    options.lastAsPrefix = true
    options.rescore = .originalFloat32
    options.alpha = 0.25
    options.rulesEnabled = true
    options.maxRounds = 0
    let plan = GraphPlan(
      root: GraphOperatorID(0), operators: [.search(GraphSearchID(0), eligibility: nil)],
      expressions: [
        .parameter(GraphParameterID(0)), .parameter(GraphParameterID(1)), .literal(.integer(1)),
      ],
      parameters: [
        GraphParameterDeclaration("vector", kinds: .list),
        GraphParameterDeclaration("text", kinds: .string),
      ],
      searches: [
        GraphSearch(
          .hybrid(vector: GraphExpressionID(0), text: GraphExpressionID(1)),
          call: GraphSearchCallID(0), k: GraphExpressionID(2), node: GraphSlotID(0),
          score: GraphSlotID(1), tier: .auto,
          vectorDistance: GraphSlotID(2), lexicalScore: GraphSlotID(3), options: options)
      ], eagerSearches: [GraphOperatorID(0)])
    _ = try await store.graphQuery(
      plan, parameters: ["vector": .list([.double(1), .double(0)]), "text": .string("amber")])
    XCTAssertEqual(probe.frees, 1)
    try await store.close()
  }
}

#endif
