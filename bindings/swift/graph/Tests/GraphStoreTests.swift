import CZeppelinEmbedGraph
import XCTest

@testable import ZeppelinEmbedGraph

final class GraphStoreTests: XCTestCase {
  func testPresenceAndListKindsRoundTrip() throws {
    let image = GraphNodeImage(
      labels: ["Doc"],
      properties: [
        "empty": .strings([]), "bools": .bools([]), "ints": .integers([]), "floats": .doubles([]),
        "sentinel": .emptyList,
      ], text: "", vector: [])
    let encoder = GraphEncoder()
    _ = try encoder.node(image)
    try encoder.withPool { pool in
      let decoder = try GraphDecoder(pool)
      let result = try decoder.node(0)
      XCTAssertEqual(result.text, "")
      XCTAssertEqual(result.vector, [])
      XCTAssertEqual(result.properties["empty"], .list(.string, []))
      XCTAssertNil(result.key)
      XCTAssertEqual(result.properties["bools"], .list(.bool, []))
      XCTAssertEqual(result.properties["ints"], .list(.integer, []))
      XCTAssertEqual(result.properties["floats"], .list(.double, []))
      XCTAssertEqual(result.properties["sentinel"], .list(.empty, []))
    }
  }
}

extension GraphStoreTests {
  func testInstalledConsumerCypherCompletesAndOwnsResult() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    var batch = GraphBatch()
    let a = batch.node(
      key: GraphKey(namespace: "", key: "a"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["title": .string("alpha")])))
    let b = batch.node(
      key: GraphKey(namespace: "", key: "b"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"])))
    batch.relationship(
      key: GraphKey(namespace: "", key: "r"), revision: 1,
      .create(GraphRelationshipImage(type: "LINK"), .local(a), .local(b)))
    _ = try await store.apply(batch)
    let result = try await store.cypher(
      "MATCH (n:Doc)-[:LINK]->(m) WHERE n.title = $title RETURN n.title, $number",
      parameters: ["title": .string("alpha"), "number": .integer(42)])
    try await store.close()
    XCTAssertEqual(result.rows, [[.string("alpha"), .integer(42)]])
    // One store counter: enable_graph commits 1; the first graph write and its reads use 2.
    XCTAssertEqual(result.metadata.admittedGeneration, 2)
  }

  func testCompletedResultsRemainUsableAfterClose() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer {
      if FileManager.default.fileExists(atPath: path.path) {
        try? FileManager.default.removeItem(at: path)
      }
    }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    var batch = GraphBatch()
    let a = batch.node(
      key: GraphKey(namespace: "", key: "a"), revision: 1,
      .create(
        GraphNodeImage(
          labels: ["Doc"], properties: ["title": .string("hello"), "empty": .integers([])])))
    let b = batch.node(
      key: GraphKey(namespace: "", key: "b"), revision: 1, .create(GraphNodeImage(labels: ["Doc"])))
    batch.relationship(
      key: GraphKey(namespace: "", key: "r"), revision: 1,
      .create(GraphRelationshipImage(type: "LINK"), .local(a), .local(b)))
    let written = try await store.apply(batch)
    XCTAssertEqual(written.metadata.disposition, .committed)
    XCTAssertEqual(written.metadata.receipts.map(\.item), [0, 1, 2])
    let replay = try await store.apply(batch)
    XCTAssertEqual(replay.metadata.disposition, .replayed)
    XCTAssertEqual(
      replay.metadata.receipts.map(\.generation), written.metadata.receipts.map(\.generation))
    let result = try await store.cypher(
      "MATCH (n:Doc) WHERE n.title = $title RETURN n, $number AS number",
      parameters: ["title": .string("hello"), "number": .integer(42)])
    let values = try await store.cypher(
      "MATCH (n:Doc) WHERE n.title = $title RETURN $null, $empty, [1, null, ['nested']], []",
      parameters: ["title": .string("hello"), "null": .null, "empty": .string("")])
    try await store.close()
    XCTAssertEqual(
      values.rows.first,
      [
        .null, .string(""),
        .list(.query, [.integer(1), .null, .list(.query, [.string("nested")])]), .list(.query, []),
      ])
    XCTAssertEqual(result.rows.count, 1)
    XCTAssertEqual(result.rows.first?.last, .integer(42))
    guard case .node(let node) = result.rows[0][0] else { return XCTFail("node") }
    XCTAssertEqual(node.key, GraphKey(namespace: "", key: "a"))
    XCTAssertEqual(node.properties["empty"], .list(.integer, []))
    XCTAssertEqual(node.revision, 1)
    XCTAssertNil(node.text)
    XCTAssertNil(node.vector)
  }
  func testQueryBoundsRejectWithoutPartialResults() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer {
      if FileManager.default.fileExists(atPath: path.path) {
        try? FileManager.default.removeItem(at: path)
      }
    }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    _ = try await store.cypher("CREATE (:Doc), (:Doc)")
    do {
      _ = try await store.cypher("MATCH (n) RETURN n", controls: GraphControls(rowLimit: 1))
      XCTFail("row limit")
    } catch let error as GraphError {
      XCTAssertEqual(error.metadata?.disposition, .notCommitted)
      XCTAssertEqual(error.code, .budgetExceeded)
    }
    try await store.close()
  }
}

extension GraphStoreTests {
  func testFailedDecodeFreesResponseExactlyOnce() throws {
    // Honest owned arrays; corrupt the Swift decoder view, never native free.
    var frees = 0
    for iteration in 0..<20 {
      let encoder = GraphEncoder()
      _ = try encoder.property(.strings(["wrong scalar for integer list"]))
      switch iteration % 5 {
      case 0: encoder.values[1].list_kind = GraphListKind.integer.rawValue
      case 1: encoder.values[1].range.start = UInt32.max
      case 2: encoder.bytes[0] = 0xff
      case 3: encoder.values[1].tag = UInt32.max
      default: encoder.values[1].list_kind = UInt32.max
      }
      let columnName = try encoder.string("x")
      try encoder.withPool { pool in
        var column = ZeGraphColumn()
        column.name = columnName
        var cell: UInt32 = 1
        try withUnsafePointer(to: &column) { column in
          try withUnsafePointer(to: &cell) { cell in
            XCTAssertThrowsError(
              try graphResponse(
                call: { response in
                  response.pool = pool
                  response.columns = column
                  response.column_count = 1
                  response.cells = cell
                  response.cell_count = 1
                  response.row_count = 1
                  response.disposition = 2
                  response.has_changed_generation = 1
                  response.changed_generation = 19
                  return 0
                },
                free: { _ in
                  frees += 1
                  return 0
                })
            ) { error in
              XCTAssertEqual((error as? GraphError)?.metadata?.changedGeneration, 19)
            }
          }
        }
      }
    }
    XCTAssertEqual(frees, 20)
  }
}

extension GraphStoreTests {
  func testErrorPreservesDispositionAndReportsBeforeThrowing() throws {
    let encoder = GraphEncoder()
    let message = try encoder.string("known committed failure")
    var diagnostic = ZeGraphDiagnostic()
    diagnostic.message = message
    diagnostic.code = 9
    var report = ZeGraphSearchReport()
    report.call_id = 7
    report.coverage = 1
    report.normalization_version = 3
    report.rules_version = 4
    report.has_document_epoch = 1
    report.document_epoch = 77
    var receipt = ZeGraphReceipt()
    receipt.disposition = 2
    receipt.node = ZeNodeId(high: UInt64.max, low: 9)
    receipt.generation = 42
    var frees = 0
    try encoder.withPool { pool in
      try withUnsafePointer(to: &diagnostic) { diagnostic in
        try withUnsafePointer(to: &report) { report in
          try withUnsafePointer(to: &receipt) { receipt in
            do {
              _ = try graphResponse(
                call: { response in
                  response.pool = pool
                  response.disposition = 2
                  response.has_changed_generation = 1
                  response.changed_generation = 42
                  response.receipts = receipt
                  response.receipt_count = 1
                  response.reports = report
                  response.report_count = 1
                  response.diagnostics = diagnostic
                  response.diagnostic_count = 1
                  return 9
                },
                free: { _ in
                  frees += 1
                  return 0
                })
              XCTFail("error")
            } catch let error as GraphError {
              XCTAssertEqual(error.code, .io)
              XCTAssertEqual(error.metadata?.changedGeneration, 42)
              XCTAssertEqual(error.metadata?.disposition, .committed)
              XCTAssertEqual(
                error.metadata?.receipts.first?.identity, .node(GraphNodeID(high: .max, low: 9)))
              XCTAssertEqual(error.metadata?.reports.first?.document_epoch, 77)
              XCTAssertEqual(error.metadata?.reports.first?.coverage, 1)
              XCTAssertEqual(error.metadata?.diagnostics.first?.message, "known committed failure")
            }
          }
        }
      }
    }
    XCTAssertEqual(frees, 1)
  }
}

private final class CallBarrier: @unchecked Sendable {
  let entered = DispatchSemaphore(value: 0), finish = DispatchSemaphore(value: 0)
  private let lock = NSLock()
  private var count = 0
  func first() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    count += 1
    return count == 1
  }
  func pause() {
    entered.signal()
    _ = finish.wait(timeout: .now() + 10)
  }
  private func waitSynchronously() -> Bool { entered.wait(timeout: .now() + 10) == .success }
  func waitForEntry() async -> Bool { await Task.detached { self.waitSynchronously() }.value }
}

extension GraphStoreTests {
  func testCancellationDoesNotOverwriteCommittedDisposition() async throws {
    for status: Int32 in [0, 9] {
      let barrier = CallBarrier()
      let token = try GraphCancellationToken()
      let calls = GraphNativeCalls(
        apply: { _, _, response in
          barrier.pause()
          response.pointee.disposition = 2
          response.pointee.has_changed_generation = 1
          response.pointee.changed_generation = 99
          return status
        }, close: { _ in 0 }, free: { _ in 0 })
      let store = ZeppelinGraphStore(token: 1, calls: calls)
      var batch = GraphBatch()
      batch.node(key: GraphKey(namespace: "", key: "x"), revision: 1, .create(GraphNodeImage()))
      let request = batch
      let task = Task { try await store.apply(request, interruption: .cancellation(token)) }
      let entered = await barrier.waitForEntry()
      XCTAssertTrue(entered)
      task.cancel()
      try token.cancel()
      barrier.finish.signal()
      do {
        let result = try await task.value
        XCTAssertEqual(status, 0)
        XCTAssertEqual(result.metadata.disposition, .committed)
        XCTAssertEqual(result.metadata.changedGeneration, 99)
      } catch let error as GraphError {
        XCTAssertEqual(status, 9)
        XCTAssertEqual(error.metadata?.disposition, .committed)
        XCTAssertEqual(error.metadata?.changedGeneration, 99)
      }
      try await store.close()
    }
  }
  func testReentrantWritePreservesBusyAndCloseBehavior() async throws {
    let write = CallBarrier()
    let close = CallBarrier()
    let calls = GraphNativeCalls(
      apply: { _, _, response in
        if write.first() {
          write.pause()
          response.pointee.disposition = 2
          return 0
        }
        response.pointee.disposition = 1
        return 7
      },
      close: { _ in
        close.pause()
        return 0
      }, free: { _ in 0 })
    let store = ZeppelinGraphStore(token: 1, calls: calls)
    var batch = GraphBatch()
    batch.node(key: GraphKey(namespace: "", key: "x"), revision: 1, .create(GraphNodeImage()))
    let request = batch
    let first = Task { try await store.apply(request) }
    let entered = await write.waitForEntry()
    XCTAssertTrue(entered)
    do {
      _ = try await store.apply(request)
      XCTFail("Busy")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .busy)
      XCTAssertEqual(error.metadata?.disposition, .notCommitted)
    }
    let closing = Task { try await store.close() }
    let closeEntered = await close.waitForEntry()
    XCTAssertTrue(closeEntered)
    do {
      _ = try await store.apply(request)
      XCTFail("closing admission")
    } catch let error as GraphError { XCTAssertEqual(error.code, .closing) }
    write.finish.signal()
    _ = try await first.value
    close.finish.signal()
    try await closing.value
    do {
      _ = try await store.apply(request)
      XCTFail("closed admission")
    } catch let error as GraphError {
      guard case .closed = error.reason else { return XCTFail("closed") }
    }
  }
}

extension GraphStoreTests {
  func testMalformedPoolStillPreservesKnownCommit() throws {
    var frees = 0
    do {
      _ = try graphResponse(
        call: { response in
          response.disposition = 2
          response.has_changed_generation = 1
          response.changed_generation = 91
          response.pool.values = nil
          response.pool.value_count = 1
          return 0
        },
        free: { _ in
          frees += 1
          return 0
        })
      XCTFail("malformed pool")
    } catch let error as GraphError {
      XCTAssertEqual(error.metadata?.disposition, .committed)
      XCTAssertEqual(error.metadata?.changedGeneration, 91)
    }
    XCTAssertEqual(frees, 1)
  }
}

extension GraphStoreTests {
  func testRustCFixtureParity() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer {
      if FileManager.default.fileExists(atPath: path.path) {
        try? FileManager.default.removeItem(at: path)
      }
    }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let nodeKey = GraphKey(namespace: "", key: "")
    let relKey = GraphKey(namespace: "", key: "rel")
    var create = GraphBatch()
    let local = create.node(
      key: nodeKey, revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["value": .integer(1)])))
    create.relationship(
      key: relKey, revision: 1,
      .create(GraphRelationshipImage(type: "LINK"), .local(local), .local(local)))
    let created = try await store.apply(create)
    guard case .node(let nodeID) = created.metadata.receipts[0].identity,
      case .relationship(let relID) = created.metadata.receipts[1].identity
    else { return XCTFail("receipt kinds") }
    var put = GraphBatch()
    put.node(
      key: nodeKey, revision: 2,
      .put(nodeID, GraphNodeImage(labels: ["Doc"], properties: ["value": .integer(2)])))
    put.relationship(
      key: relKey, revision: 2,
      .put(
        relID, GraphRelationshipImage(type: "LINK", properties: ["value": .string("updated")]),
        .node(nodeID), .node(nodeID)))
    _ = try await store.apply(put)
    let rows = try await store.cypher("MATCH (n:Doc)-[r:LINK]->(m) RETURN n, r")
    guard case .node(let node) = rows.rows[0][0], case .relationship(let rel) = rows.rows[0][1]
    else { return XCTFail("entity rows") }
    XCTAssertEqual(node.key, nodeKey)
    XCTAssertEqual(node.revision, 2)
    XCTAssertEqual(node.properties["value"], .integer(2))
    XCTAssertEqual(rel.id, relID)
    XCTAssertEqual(rel.source, nodeID)
    XCTAssertEqual(rel.target, nodeID)
    XCTAssertEqual(rel.key, relKey)
    XCTAssertEqual(rel.properties["value"], .string("updated"))
    _ = try await store.cypher(
      "MATCH (n:Doc) SET n.value = $value RETURN n.value", parameters: ["value": .integer(3)])
    var deletion = GraphBatch()
    deletion.relationship(key: relKey, revision: 3, .delete(relID))
    deletion.node(key: nodeKey, revision: 4, .delete(nodeID, detach: false))
    let deleted = try await store.apply(deletion)
    XCTAssertTrue(deleted.metadata.receipts.allSatisfy(\.deleted))
    var recreate = GraphBatch()
    let newLocal = recreate.node(
      key: nodeKey, revision: 5, .recreate(deletionRevision: 4, GraphNodeImage(labels: ["Doc"])))
    recreate.relationship(
      key: relKey, revision: 4,
      .recreate(
        deletionRevision: 3, GraphRelationshipImage(type: "LINK"), .local(newLocal),
        .local(newLocal)))
    let recreated = try await store.apply(recreate)
    XCTAssertNotEqual(recreated.metadata.receipts[0].identity, .node(nodeID))
    XCTAssertNotEqual(recreated.metadata.receipts[1].identity, .relationship(relID))
    try await store.close()
    XCTAssertEqual(rel.type, "LINK")
  }
}

extension GraphStoreTests {
  func testExplicitCancellationAndCompilerBounds() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer {
      if FileManager.default.fileExists(atPath: path.path) {
        try? FileManager.default.removeItem(at: path)
      }
    }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let token = try GraphCancellationToken()
    try token.cancel()
    var batch = GraphBatch()
    batch.node(key: GraphKey(namespace: "", key: "x"), revision: 1, .create(GraphNodeImage()))
    do {
      _ = try await store.apply(batch, interruption: .cancellation(token))
      XCTFail("cancellation")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .cancelled)
      XCTAssertEqual(error.metadata?.disposition, .notCommitted)
      XCTAssertNil(error.metadata?.changedGeneration)
    }
    var limits = GraphCompileLimits()
    limits.textBytes = 1
    do {
      _ = try await store.cypher("CREATE (:Doc)", controls: GraphControls(compileLimits: limits))
      XCTFail("compiler bounds")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .budgetExceeded)
      XCTAssertNil(error.metadata?.changedGeneration)
    }
    let result = try await store.cypher("MATCH (n) RETURN n")
    XCTAssertEqual(result.rows.count, 0)
    try await store.close()
  }
}

extension GraphStoreTests {
  func testDocumentTowerAllowsVectorBatchAndRejectsPresentEmpty() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer {
      if FileManager.default.fileExists(atPath: path.path) {
        try? FileManager.default.removeItem(at: path)
      }
    }
    let tower = EmbeddingTower(
      modelID: "fixture", modelVersion: "1", weightsDigest: Data([1]), dimensions: 2, maxTokens: 10,
      runtime: .cpuReference, computeUnits: .cpu, operatingSystemBuild: "")
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create, documentTower: tower)
    var valid = GraphBatch()
    valid.node(
      key: GraphKey(namespace: "", key: "v"), revision: 1,
      .create(GraphNodeImage(text: "", vector: [1, 0])))
    let result = try await store.apply(valid)
    XCTAssertEqual(result.metadata.disposition, .committed)
    let replay = try await store.apply(valid)
    XCTAssertEqual(replay.metadata.disposition, .replayed)
    var empty = GraphBatch()
    empty.node(
      key: GraphKey(namespace: "", key: "empty"), revision: 1, .create(GraphNodeImage(vector: [])))
    do {
      _ = try await store.apply(empty)
      XCTFail("present empty vector is not absence")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .dimensionMismatch)
      XCTAssertEqual(error.metadata?.disposition, .notCommitted)
    }
    try await store.close()
  }
}

extension GraphStoreTests {
  func testZE76ResourceParity() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let observed = try await store.resources()
    XCTAssertGreaterThan(observed.engineBytes, 0)
    XCTAssertGreaterThanOrEqual(observed.enginePeakBytes, observed.engineBytes)
    XCTAssertEqual(observed.applicationBytes, 0)
    XCTAssertEqual(observed.applicationPeakBytes, 0)
    try await store.close()
  }
}
