import XCTest

@testable import ZeppelinEmbedGraph

final class GraphGetterTests: XCTestCase {
  func testGettersPreserveOrderPresenceAndOwnershipAfterReopen() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let tower = EmbeddingTower(
      modelID: "fixture", modelVersion: "1", weightsDigest: Data([1]),
      dimensions: 2, maxTokens: 10, runtime: .cpuReference, computeUnits: .cpu)
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create, documentTower: tower)
    var batch = GraphBatch()
    let a = batch.node(
      key: GraphKey(namespace: "app", key: "a"), revision: 7,
      .create(
        GraphNodeImage(
          labels: ["Doc"],
          properties: [
            "bools": .bools([]), "ints": .integers([]),
            "floats": .doubles([]), "strings": .strings([]), "empty": .emptyList,
          ], text: "", vector: [1, 0])))
    let b = batch.node(
      key: GraphKey(namespace: "app", key: "b"), revision: 1, .create(GraphNodeImage()))
    batch.relationship(
      key: GraphKey(namespace: "app", key: "r"), revision: 3,
      .create(
        GraphRelationshipImage(type: "LINK", properties: ["empty": .integers([])]), .local(a),
        .local(b)))
    let written = try await store.apply(batch)
    guard case .node(let aID) = written.metadata.receipts[0].identity,
      case .node(let bID) = written.metadata.receipts[1].identity,
      case .relationship(let rID) = written.metadata.receipts[2].identity
    else { return XCTFail("identities") }
    let missing = GraphNodeID(high: .max, low: .max)
    let selected = try await store.getNodes(
      [aID, missing, bID, aID], fields: GraphNodeFields(text: true, vector: true))
    XCTAssertEqual(selected.nodes.map { $0?.id }, [aID, nil, bID, aID])
    XCTAssertEqual(selected.metadata.admittedGeneration, written.metadata.changedGeneration)
    let omitted = try await store.getNodes([aID])
    XCTAssertNil(omitted.nodes[0]?.text)
    XCTAssertNil(omitted.nodes[0]?.vector)
    let relationships = try await store.getRelationships([
      rID, GraphRelationshipID(high: .max, low: .max), rID,
    ])
    XCTAssertEqual(relationships.relationships.map { $0?.id }, [rID, nil, rID])
    let emptyNodes = try await store.getNodes([])
    let emptyRelationships = try await store.getRelationships([])
    XCTAssertTrue(emptyNodes.nodes.isEmpty)
    XCTAssertTrue(emptyRelationships.relationships.isEmpty)
    XCTAssertEqual(emptyNodes.metadata.admittedGeneration, written.metadata.changedGeneration)
    try await store.close()
    let reopened = try await ZeppelinGraphStore.open(at: path, documentTower: tower)
    let again = try await reopened.getNodes(
      [aID], fields: GraphNodeFields(text: true, vector: true))
    try await reopened.close()
    XCTAssertEqual(again.nodes[0], selected.nodes[0])
    XCTAssertEqual(selected.nodes[0]?.text, "")
    XCTAssertNil(selected.nodes[2]?.text)
    XCTAssertEqual(selected.nodes[0]?.vector, [1, 0])
    XCTAssertNil(selected.nodes[2]?.vector)
    XCTAssertEqual(selected.nodes[0]?.key, GraphKey(namespace: "app", key: "a"))
    XCTAssertEqual(selected.nodes[0]?.revision, 7)
    XCTAssertEqual(selected.nodes[0]?.lastChangeGeneration, written.metadata.changedGeneration)
    for (name, kind) in [
      ("bools", GraphListKind.bool), ("ints", .integer), ("floats", .double), ("strings", .string),
      ("empty", .empty),
    ] {
      XCTAssertEqual(selected.nodes[0]?.properties[name], .list(kind, []))
    }
    XCTAssertNil(selected.nodes[0]?.properties["absent"])
    XCTAssertEqual(relationships.relationships[0]?.source, aID)
    XCTAssertEqual(relationships.relationships[0]?.target, bID)
    XCTAssertEqual(relationships.relationships[0]?.type, "LINK")
    XCTAssertEqual(relationships.relationships[0]?.revision, 3)
    XCTAssertEqual(relationships.relationships[0]?.key, GraphKey(namespace: "app", key: "r"))
    XCTAssertEqual(relationships.relationships[0]?.properties["empty"], .list(.integer, []))
  }
  func testEmptyGetterRequestsStillValidateLimitsAndCancellation() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    let token = try GraphCancellationToken()
    try token.cancel()
    for nodes in [true, false] {
      do {
        if nodes {
          _ = try await store.getNodes([], interruption: .cancellation(token))
        } else {
          _ = try await store.getRelationships([], interruption: .cancellation(token))
        }
        XCTFail("empty request must reach native cancellation validation")
      } catch let error as GraphError { XCTAssertEqual(error.code, .cancelled) }
      do {
        let limits = GraphQueryLimits(queryBytes: 25 * 1024 * 1024)
        if nodes {
          _ = try await store.getNodes([], limits: limits)
        } else {
          _ = try await store.getRelationships([], limits: limits)
        }
        XCTFail("empty request must reach native limit validation")
      } catch let error as GraphError { XCTAssertEqual(error.code, .invalidArgument) }
    }
    try await store.close()
  }
}
