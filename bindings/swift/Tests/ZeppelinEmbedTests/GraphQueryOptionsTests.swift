#if ZE_GRAPH
import XCTest

@testable import ZeppelinEmbed

final class GraphQueryOptionsTests: XCTestCase {
  func testStructuredAndCypherShareMemoryAndWorkRefusals() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    var batch = GraphBatch()
    batch.node(key: GraphKey(namespace: "", key: "a"), revision: 1, .create(GraphNodeImage()))
    _ = try await store.graphApply(batch)
    let plan = GraphPlan(
      root: GraphOperatorID(0), operators: [.scanNodes(output: GraphSlotID(0), label: nil)])
    for limits in [GraphQueryLimits(queryBytes: 0), GraphQueryLimits(work: [.completedRows: 0])] {
      let options = GraphQueryOptions(limits: limits)
      for structured in [true, false] {
        do {
          if structured {
            _ = try await store.graphQuery(plan, options: options)
          } else {
            _ = try await store.cypher("MATCH (n) RETURN n", options: options)
          }
          XCTFail("query limits must refuse, never return partial rows")
        } catch let error as GraphError {
          XCTAssertEqual(error.code, .budgetExceeded)
          XCTAssertNil(error.metadata?.changedGeneration)
          XCTAssertNotEqual(error.metadata?.disposition, .committed)
        }
      }
    }
    let rows = try await store.graphQuery(plan)
    XCTAssertEqual(rows.rows.count, 1)
    try await store.close()
  }
  func testQueryTowerCompatibilityAndExplicitAlignment() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let tower = EmbeddingTower(
      modelID: "fixture", modelVersion: "1", weightsDigest: Data([1]),
      dimensions: 2, maxTokens: 10, runtime: .cpuReference, computeUnits: .cpu)
    let store = try await openGraphTestStore(at: path, mode: .create, documentTower: tower)
    var batch = GraphBatch()
    batch.node(
      key: GraphKey(namespace: "", key: "v"), revision: 1, .create(GraphNodeImage(vector: [1, 0])))
    _ = try await store.graphApply(batch)
    let plan = GraphPlan(
      root: GraphOperatorID(0), operators: [.search(GraphSearchID(0), eligibility: nil)],
      expressions: [.literal(.list([.double(1), .double(0)])), .literal(.integer(1))],
      searches: [
        GraphSearch(
          .vector(GraphExpressionID(0)), call: GraphSearchCallID(0), k: GraphExpressionID(1),
          node: GraphSlotID(0), score: GraphSlotID(1), tier: .exact)
      ], eagerSearches: [GraphOperatorID(0)])
    var asymmetric = tower
    asymmetric.modelID = "query"
    var wrongGeometry = tower
    wrongGeometry.dimensions = 3
    for structured in [true, false] {
      for options in [
        GraphQueryOptions(queryTower: tower),
        GraphQueryOptions(queryTower: asymmetric, alignmentDigest: Data([9])),
      ] {
        let result: GraphResult
        if structured {
          result = try await store.graphQuery(plan, options: options)
        } else {
          result = try await store.cypher(
            "CALL ze.vector_search($vector, 1, 'exact') YIELD node, distance RETURN distance",
            parameters: ["vector": .list([.double(1), .double(0)])], options: options)
        }
        XCTAssertEqual(result.rows.count, 1)
        XCTAssertEqual(result.metadata.reports.count, 1)
      }
      for options in [
        GraphQueryOptions(queryTower: asymmetric),
        GraphQueryOptions(queryTower: wrongGeometry, alignmentDigest: Data([9])),
      ] {
        do {
          if structured {
            _ = try await store.graphQuery(plan, options: options)
          } else {
            _ = try await store.cypher("RETURN 1", options: options)
          }
          XCTFail("tower geometry and pairing must validate")
        } catch let error as GraphError { XCTAssertEqual(error.code, .epochMismatch) }
      }
      do {
        let options = GraphQueryOptions(alignmentDigest: Data([9]))
        if structured {
          _ = try await store.graphQuery(plan, options: options)
        } else {
          _ = try await store.cypher("RETURN 1", options: options)
        }
        XCTFail("alignment without tower must reject")
      } catch let error as GraphError { XCTAssertEqual(error.code, .invalidArgument) }
    }
    try await store.close()
  }
}

#endif
