import CZeppelinEmbedGraph
import Foundation
import XCTest
@testable import ZeppelinEmbedGraph

// These adapters read the same versioned TSV as Rust/C; no native outcome is fabricated.
final class GraphBindingsParityTests: XCTestCase {
  private var root: URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent()
      .appendingPathComponent("../../../..").standardizedFileURL
  }
  private func cell(_ value: GraphValue) -> String {
    switch value {
    case .null: return "N"
    case .bool(let b): return "B\(b ? 1 : 0)"
    case .integer(let i): return "I\(i)"
    case .double(let f): return String(format: "F%016llx", f.bitPattern)
    case .string(let s): return "S\(s.utf8.count):\(s)"
    case .node(let n): return String(format: "D%016llx%016llx", n.id.high, n.id.low)
    case .relationship(let r): return String(format: "R%016llx%016llx", r.id.high, r.id.low)
    case .list(let kind, let values):
      return "L\(kind.rawValue)[\(values.map(cell).joined(separator: ","))]"
    }
  }
  private func observe(_ result: GraphResult) -> String {
    result.rows.map { $0.map(cell).joined(separator: "|") }.joined(separator: "\n")
  }
  func testSharedSemantics() async throws {
    let fixture = root.appendingPathComponent("tests/fixtures/graph-bindings-v1/semantics.tsv")
    let lines = try String(contentsOf: fixture, encoding: .utf8).split(separator: "\n")
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let noop = try await store.apply(GraphBatch())
    XCTAssertEqual(noop.metadata.disposition, .noOp)
    XCTAssertNil(noop.metadata.changedGeneration)
    _ = try await store.cypher("CREATE (:Fixture), (:Fixture)")
    var retained: [(String, GraphResult)] = []
    for line in lines where !line.hasPrefix("#") {
      let fields = line.split(separator: "\t", omittingEmptySubsequences: false)
      XCTAssertEqual(fields.count, 3)
      let expected = fields[2].replacingOccurrences(of: "\\n", with: "\n")
      let result = try await store.cypher(String(fields[1]), controls: GraphControls(rowLimit: 1024))
      XCTAssertEqual(observe(result), expected, String(fields[0]))
      retained.append((expected, result))
    }
    try await store.close()
    let reopened = try await ZeppelinGraphStore.open(at: path, mode: .readWrite)
    for (expected, result) in retained { XCTAssertEqual(observe(result), expected) }
    try await reopened.close()
  }
  func testApplicationSemantics() async throws {
    guard let observations = ProcessInfo.processInfo.environment["ZE72_CORPUS_OUTPUT"] else {
      throw XCTSkip("ZE-72 shared application corpus absent: run scripts/qualify-graph-bindings.sh")
    }
    let path = try String(contentsOfFile: observations + ".path", encoding: .utf8)
    let tower = EmbeddingTower(
      modelID: "ze41-document", modelVersion: "1", weightsDigest: Data([0x41, 0xa5]),
      dimensions: 2, normalization: .none, promptPrefix: "doc: ", maxTokens: 32,
      runtime: .cpuReference, computeUnits: .cpu)
    let store = try await ZeppelinGraphStore.open(
      at: URL(fileURLWithPath: path), mode: .readWrite, documentTower: tower)
    for line in try String(contentsOfFile: observations, encoding: .utf8).split(separator: "\n") {
      let fields = line.split(separator: "\t", omittingEmptySubsequences: false)
      XCTAssertEqual(fields.count, 5)
      let result = try await store.cypher(String(fields[1]), controls: GraphControls(rowLimit: 1024))
      let observed = observe(result)
      print("ZE72 observed\t\(fields[0])\t\(observed.replacingOccurrences(of: "\n", with: "\\n"))\t\(result.metadata.reports.count)\t\(result.metadata.admittedGeneration)")
      XCTAssertEqual(observed, fields[2].replacingOccurrences(of: "\\n", with: "\n"), String(fields[0]))
      XCTAssertEqual(result.metadata.reports.count, Int(fields[3]))
      XCTAssertEqual(result.metadata.admittedGeneration, UInt64(fields[4]))
      for report in result.metadata.reports {
        XCTAssertEqual(report.generation, UInt64(fields[4]))
        XCTAssertEqual(report.call_id, 0)
        XCTAssertEqual(report.kind, 0)
      }
    }
    try await store.close()
  }
  func testHighBitIDsAndMetadata() async throws {
    guard let output = ProcessInfo.processInfo.environment["ZE72_CORPUS_OUTPUT"] else {
      throw XCTSkip("ZE-72 shared high-bit corpus absent: run scripts/qualify-graph-bindings.sh")
    }
    let path = try String(contentsOfFile: output + ".twins.path", encoding: .utf8)
    let expected = try String(contentsOfFile: output + ".twins", encoding: .utf8)
    let store = try await ZeppelinGraphStore.open(at: URL(fileURLWithPath: path), mode: .readWrite)
    let result = try await store.cypher("MATCH (n:Twin) RETURN n ORDER BY n")
    print("ZE72 observed\ttwins\t\(observe(result).replacingOccurrences(of: "\n", with: "\\n"))")
    XCTAssertEqual(observe(result), expected)
    XCTAssertEqual(result.rows.count, 2)
    for row in result.rows {
      guard case .node(let n) = row[0] else { return XCTFail("entity result lost") }
      XCTAssertEqual(n.id.low, 7)
      XCTAssertEqual(n.revision, 1)
      XCTAssertNil(n.key)
      XCTAssertNil(n.text)
      XCTAssertNil(n.vector)
      XCTAssertEqual(n.labels, ["Twin"])
    }
    try await store.close()
    XCTAssertEqual(observe(result), expected)
  }
  func testStoredPropertiesAndReplay() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    var batch = GraphBatch()
    let fixture = root.appendingPathComponent("tests/fixtures/graph-bindings-v1/stored-properties.tsv")
    let cases = try String(contentsOf: fixture, encoding: .utf8).split(separator: "\n")
      .filter { !$0.hasPrefix("#") }.map { $0.split(separator: "\t") }
    var properties: [String: GraphProperty] = [:]
    for fields in cases {
      switch fields[1] {
      case "1": properties[String(fields[0])] = .bools([])
      case "2": properties[String(fields[0])] = .integers([])
      case "3": properties[String(fields[0])] = .doubles([])
      case "4": properties[String(fields[0])] = .strings([])
      case "5": properties[String(fields[0])] = .emptyList
      default: return XCTFail("invalid shared stored-list fixture")
      }
    }
    let image = GraphNodeImage(labels: ["Payload"], properties: properties, text: "")
    batch.node(key: GraphKey(namespace: "ze72", key: "payload"), revision: 1, .create(image))
    let written = try await store.apply(batch)
    let replay = try await store.apply(batch)
    XCTAssertEqual(written.metadata.changedGeneration, 1)
    XCTAssertEqual(replay.metadata.disposition, .replayed)
    XCTAssertNil(replay.metadata.changedGeneration)
    XCTAssertEqual(replay.metadata.receipts.map(\.generation), [1])
    let result = try await store.cypher("MATCH (n:Payload) RETURN n, ze.stored_text(n)")
    guard case .node(let node) = result.rows[0][0] else { return XCTFail("node result") }
    XCTAssertEqual(result.rows[0][1], .string(""))
    for fields in cases {
      guard let value = node.properties[String(fields[0])] else { return XCTFail("missing stored property") }
      XCTAssertEqual(cell(value), String(fields[2]), String(fields[0]))
    }
    XCTAssertEqual(node.revision, 1)
    XCTAssertEqual(node.lastChangeGeneration, 1)
    try await store.close()
    XCTAssertEqual(result.rows[0][1], .string(""))
  }
  func testStructuredQueryHandoff() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let key = GraphKey(namespace: "handoff", key: "first")
    var batch = GraphBatch()
    batch.node(
      key: key, revision: 1,
      .create(GraphNodeImage(labels: ["Handoff"], properties: ["title": .string("hello")], text: "")))
    let written = try await store.apply(batch)
    XCTAssertEqual(written.metadata.disposition, .committed)
    guard let receipt = written.metadata.receipts.first, case .node(let id) = receipt.identity else {
      return XCTFail("node receipt missing")
    }
    let plan = GraphPlan(
      root: GraphOperatorID(1),
      operators: [
        .scanNodes(output: GraphSlotID(0), label: "Handoff"),
        .project(
          input: GraphOperatorID(0),
          bindings: [GraphProjection(GraphSlotID(7), GraphExpressionID(1))]),
      ],
      expressions: [.slot(GraphSlotID(0)), .property(GraphExpressionID(0), "title")])
    let result = try await store.query(plan)
    let fetched = try await store.getNodes([id], fields: GraphNodeFields(text: true))
    try await store.close()
    XCTAssertEqual(result.columns.map(\.name), ["slot_7"])
    XCTAssertEqual(result.rows, [[.string("hello")]])
    XCTAssertEqual(result.metadata.admittedGeneration, written.metadata.changedGeneration)
    XCTAssertEqual(fetched.metadata.admittedGeneration, written.metadata.changedGeneration)
    XCTAssertEqual(fetched.nodes.count, 1)
    let node = try XCTUnwrap(fetched.nodes.first ?? nil)
    XCTAssertEqual(node.id, id)
    XCTAssertEqual(node.key, key)
    XCTAssertEqual(node.labels, ["Handoff"])
    XCTAssertEqual(node.properties["title"], .string("hello"))
    XCTAssertEqual(node.text, "")
    XCTAssertEqual(node.revision, 1)
  }
}

#if ZE72_TEST_BRIDGE
@_silgen_name("ze72_test_cypher")
private func ze72TestCypher(
  _ handle: ZeGraphHandle, _ request: UnsafePointer<ZeGraphCypherRequest>,
  _ response: UnsafeMutablePointer<ZeGraphResponse>, _ mode: UInt32,
  _ fires: UnsafeMutablePointer<UInt64>
) -> Int32
extension GraphBindingsParityTests {
  func testRealFaultBoundaries() throws {
    // Calls and fault installation occur on this same blocking thread.
    for mode: UInt32 in 0...6 {
      let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: path) }
      var handle = ZeGraphHandle()
      var open = ZeGraphOpenRequest()
      open.abi_size = graphSize(ZeGraphOpenRequest.self)
      open.max_resident_bytes = 256 << 20
      let bytes = Array(path.path.utf8)
      try bytes.withUnsafeBufferPointer { buffer in
        open.path = ZeGraphBytes(data: buffer.baseAddress, count: buffer.count)
        XCTAssertEqual(ze_graph_open(&open, &handle), 0)
        let query = Array("CREATE (:Fault)".utf8)
        var fires: UInt64 = 0
        var metadata: GraphMetadata?
        try query.withUnsafeBufferPointer { text in
          var request = ZeGraphCypherRequest()
          request.abi_size = graphSize(ZeGraphCypherRequest.self)
          request.query = ZeGraphBytes(data: text.baseAddress, count: text.count)
          do {
            metadata = try graphResponse { response in
              ze72TestCypher(handle, &request, &response, mode, &fires)
            }.metadata
            XCTAssertTrue(mode == 0 || mode == 4)
          } catch let error as GraphError {
            metadata = error.metadata
            XCTAssertTrue(mode != 0 && mode != 4)
            XCTAssertEqual(error.code, mode == 1 ? .outOfMemory : (mode == 5 || mode == 6) ? .panic : .indeterminateCommit)
          }
        }
        XCTAssertEqual(fires, mode == 0 ? 0 : 1)
        let disposition: GraphDisposition = mode == 1 ? .notCommitted
          : (mode == 2 || mode == 3 || mode == 5) ? .indeterminate : .committed
        XCTAssertEqual(metadata?.disposition, disposition)
        let close = ze_graph_close(handle)
        XCTAssertTrue(close == 0 || mode == 5 || mode == 6)
        open.mode = 1
        XCTAssertEqual(ze_graph_open(&open, &handle), 0)
        let readQuery = Array("MATCH (n:Fault) RETURN count(n)".utf8)
        try readQuery.withUnsafeBufferPointer { text in
          var request = ZeGraphCypherRequest()
          request.abi_size = graphSize(ZeGraphCypherRequest.self)
          request.query = ZeGraphBytes(data: text.baseAddress, count: text.count)
          let result = try graphResponse { ze_graph_cypher(handle, &request, &$0) }
          XCTAssertEqual(result.rows, [[.integer(mode == 1 || mode == 2 ? 0 : 1)]])
        }
        XCTAssertEqual(ze_graph_close(handle), 0)
      }
    }
  }
}
#else
extension GraphBindingsParityTests {
  func testRealFaultBoundaries() throws {
    throw XCTSkip("ZE-72 test bridge missing: build graph-bindings-test-support and pass -Xswiftc -DZE72_TEST_BRIDGE; shipping archives exclude these hooks")
  }
}
#endif

#if ZE72_TEST_BRIDGE
private final class NativeEntryBarrier: @unchecked Sendable {
  let entered = DispatchSemaphore(value: 0)
  let release = DispatchSemaphore(value: 0)
  let closeEntered = DispatchSemaphore(value: 0)
  let busyReturned = DispatchSemaphore(value: 0)
}
@_cdecl("ze72_swift_wait_before_append")
private func waitBeforeAppend(_ context: UnsafeMutableRawPointer?) {
  let barrier = Unmanaged<NativeEntryBarrier>.fromOpaque(context!).takeUnretainedValue()
  barrier.entered.signal()
  XCTAssertEqual(barrier.release.wait(timeout: .now() + 10), .success, "ZE-72 native write release")
}
@_silgen_name("ze72_test_apply_at_append")
private func ze72TestApplyAtAppend(
  _ handle: ZeGraphHandle, _ request: UnsafePointer<ZeGraphBatchRequest>,
  _ response: UnsafeMutablePointer<ZeGraphResponse>,
  _ callback: @convention(c) (UnsafeMutableRawPointer?) -> Void,
  _ context: UnsafeMutableRawPointer?, _ fires: UnsafeMutablePointer<UInt64>
) -> Int32
extension GraphBindingsParityTests {
  func testActualReentrantWriterAndClose() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let bytes = Array(path.path.utf8)
    var handle = ZeGraphHandle()
    bytes.withUnsafeBufferPointer { buffer in
      var request = ZeGraphOpenRequest()
      request.abi_size = graphSize(ZeGraphOpenRequest.self)
      request.path = ZeGraphBytes(data: buffer.baseAddress, count: buffer.count)
      request.max_resident_bytes = 256 << 20
      XCTAssertEqual(ze_graph_open(&request, &handle), 0)
    }
    let barrier = NativeEntryBarrier()
    let calls = GraphNativeCalls(
      apply: { token, request, response in
        var fires: UInt64 = 0
        let status = ze72TestApplyAtAppend(
          ZeGraphHandle(token: token), request, response, waitBeforeAppend,
          Unmanaged.passUnretained(barrier).toOpaque(), &fires)
        XCTAssertEqual(fires, status == 0 ? 1 : 0)
        if status == ZeppelinError.busy.rawValue { barrier.busyReturned.signal() }
        return status
      },
      close: { token in
        barrier.closeEntered.signal()
        return ze_graph_close(ZeGraphHandle(token: token))
      })
    let store = ZeppelinGraphStore(token: handle.token, calls: calls)
    var first = GraphBatch()
    first.node(key: GraphKey(namespace: "ze72", key: "first"), revision: 1,
      .create(GraphNodeImage(labels: ["First"])))
    let writer = Task { try await store.apply(first) }
    XCTAssertEqual(barrier.entered.wait(timeout: .now() + 5), .success, "ZE-72 actual append reached")
    var second = GraphBatch()
    second.node(key: GraphKey(namespace: "ze72", key: "second"), revision: 1,
      .create(GraphNodeImage(labels: ["Second"])))
    // Native Busy is returned immediately; graphResponse's matching free
    // serializes behind the admitted response, so await it after release.
    let contender = Task { try await store.apply(second) }
    XCTAssertEqual(barrier.busyReturned.wait(timeout: .now() + 5), .success, "ZE-72 actual Busy returned")
    let closer = Task { try await store.close() }
    XCTAssertEqual(barrier.closeEntered.wait(timeout: .now() + 5), .success, "ZE-72 actual close reached")
    do {
      _ = try await store.apply(second)
      XCTFail("actor admitted a write while close was active")
    } catch let error as GraphError { XCTAssertEqual(error.code, .closing) }
    barrier.release.signal()
    do {
      _ = try await contender.value
      XCTFail("actual concurrent writer was accepted")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .busy)
      XCTAssertEqual(error.metadata?.disposition, .notCommitted)
    }
    let written = try await writer.value
    try await closer.value
    XCTAssertEqual(written.metadata.disposition, .committed)
    XCTAssertEqual(written.metadata.changedGeneration, 1)
    XCTAssertEqual(written.metadata.receipts.count, 1)
    let reopened = try await ZeppelinGraphStore.open(at: path, mode: .readWrite)
    let actual = try await reopened.cypher("MATCH (n) RETURN count(n)")
    XCTAssertEqual(actual.rows, [[.integer(1)]])
    try await reopened.close()
  }
}
#endif
