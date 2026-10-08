#if ZE_GRAPH
import XCTest

@testable import ZeppelinEmbed

final class GraphStructuredQueryTests: XCTestCase {
  func testStructuredQueryReturnsOwnedRows() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    var batch = GraphBatch()
    batch.node(
      key: GraphKey(namespace: "", key: "a"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["title": .string("hello")])))
    let written = try await store.graphApply(batch)
    let plan = GraphPlan(
      root: GraphOperatorID(2),
      operators: [
        .scanNodes(output: GraphSlotID(0), label: "Doc"),
        .filter(input: GraphOperatorID(0), predicate: GraphExpressionID(3)),
        .project(
          input: GraphOperatorID(1),
          bindings: [GraphProjection(GraphSlotID(7), GraphExpressionID(1))]),
      ],
      expressions: [
        .slot(GraphSlotID(0)), .property(GraphExpressionID(0), "title"),
        .parameter(GraphParameterID(0)),
        .binary(.equal, GraphExpressionID(1), GraphExpressionID(2)),
      ], parameters: [GraphParameterDeclaration("title", kinds: .string)])
    let result: GraphResult
    do { result = try await store.graphQuery(plan, parameters: ["title": .string("hello")]) } catch let
      error as GraphError
    {
      XCTFail(
        "ZE-241 retained parameter backing repair required: structured named parameters reject with unproved query input ownership on main; \(error)"
      )
      try await store.close()
      return
    }
    try await store.close()
    XCTAssertEqual(result.rows, [[.string("hello")]])
    XCTAssertEqual(result.columns.map(\.name), ["slot_7"])
    XCTAssertEqual(result.metadata.admittedGeneration, written.metadata.changedGeneration)
  }
}

extension GraphStructuredQueryTests {
  func testRelationalPathsAndAggregates() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    var batch = GraphBatch()
    let a = batch.node(
      key: GraphKey(namespace: "app", key: "a"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["value": .integer(2)])))
    let b = batch.node(
      key: GraphKey(namespace: "app", key: "b"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["value": .integer(1)])))
    batch.relationship(
      key: GraphKey(namespace: "app", key: "r"), revision: 1,
      .create(GraphRelationshipImage(type: "LINK"), .local(a), .local(b)))
    let written = try await store.graphApply(batch)
    guard case .node(let aID) = written.metadata.receipts[0].identity,
      case .node(let bID) = written.metadata.receipts[1].identity,
      case .relationship(let rID) = written.metadata.receipts[2].identity
    else { return XCTFail("identities") }
    let relational = GraphPlan(
      root: GraphOperatorID(6),
      operators: [
        .scanNodes(output: GraphSlotID(0), label: "Doc"),
        .project(
          input: GraphOperatorID(0),
          bindings: [GraphProjection(GraphSlotID(1), GraphExpressionID(1))]),
        .with(
          input: GraphOperatorID(1),
          bindings: [GraphProjection(GraphSlotID(1), GraphExpressionID(2))]),
        .distinct(input: GraphOperatorID(2)),
        .sort(input: GraphOperatorID(3), keys: [GraphSortKey(GraphExpressionID(2))]),
        .offsetLimit(input: GraphOperatorID(4), offset: 1, limit: 1),
        .collect(input: GraphOperatorID(5)),
      ],
      expressions: [
        .slot(GraphSlotID(0)), .property(GraphExpressionID(0), "value"), .slot(GraphSlotID(1)),
      ])
    let rows = try await store.graphQuery(relational)
    XCTAssertEqual(rows.rows, [[.integer(2)]])
    let aggregate = GraphPlan(
      root: GraphOperatorID(1),
      operators: [
        .scanNodes(output: GraphSlotID(0), label: nil),
        .aggregate(
          input: GraphOperatorID(0), keys: [],
          aggregates: [
            GraphProjection(GraphSlotID(7), GraphExpressionID(0)),
            GraphProjection(GraphSlotID(8), GraphExpressionID(2)),
          ]),
      ],
      expressions: [
        .aggregate(.count, operand: nil, distinct: false), .slot(GraphSlotID(0)),
        .aggregate(.collect, operand: GraphExpressionID(1), distinct: true),
      ])
    let totals = try await store.graphQuery(aggregate)
    XCTAssertEqual(totals.rows[0][0], .integer(2))
    guard case .list(.query, let collected) = totals.rows[0][1] else { return XCTFail("collect") }
    XCTAssertEqual(collected.count, 2)
    for bounded in [false, true] {
      let spec = GraphExpansion(
        source: GraphSlotID(0), node: GraphSlotID(1), relationship: GraphSlotID(2), types: ["LINK"])
      let op: GraphOperator =
        bounded
        ? .boundedExpand(
          input: GraphOperatorID(0), spec, min: 1, max: 1,
          edgePredicate: GraphEdgePredicate(slot: GraphSlotID(9), expression: GraphExpressionID(1)))
        : .expand(input: GraphOperatorID(0), spec)
      let expanded = try await store.graphQuery(
        GraphPlan(
          root: GraphOperatorID(1),
          operators: [
            .lookupNode(output: GraphSlotID(0), id: aID), op,
          ],
          expressions: bounded
            ? [.slot(GraphSlotID(9)), .unary(.isNotNull, GraphExpressionID(0))] : []))
      XCTAssertEqual(expanded.rows.count, 1)
      guard case .node(let node) = expanded.rows[0][1] else { return XCTFail("target") }
      XCTAssertEqual(node.id, bID)
    }
    let keyed = try await store.graphQuery(
      GraphPlan(
        root: GraphOperatorID(0),
        operators: [
          .lookupKey(
            output: GraphSlotID(4), kind: .node, namespace: "app", key: GraphExpressionID(0))
        ], expressions: [.literal(.string("a"))]))
    guard case .node(let keyedNode) = keyed.rows[0][0] else { return XCTFail("keyed") }
    XCTAssertEqual(keyedNode.id, aID)
    let relationship = try await store.graphQuery(
      GraphPlan(
        root: GraphOperatorID(0),
        operators: [
          .lookupRelationship(output: GraphSlotID(2), id: rID)
        ]))
    guard case .relationship(let rel) = relationship.rows[0][0] else {
      return XCTFail("relationship lookup")
    }
    XCTAssertEqual(rel.id, rID)
    for optional in [false, true] {
      let op: GraphOperator =
        optional
        ? .optionalApply(
          left: GraphOperatorID(0), right: GraphOperatorID(1), predicate: GraphExpressionID(0))
        : .join(
          left: GraphOperatorID(0), right: GraphOperatorID(1), predicate: GraphExpressionID(0))
      let joined = try await store.graphQuery(
        GraphPlan(
          root: GraphOperatorID(2),
          operators: [
            .lookupNode(output: GraphSlotID(0), id: aID),
            .lookupNode(output: GraphSlotID(1), id: bID), op,
          ], expressions: [.literal(.bool(false))]))
      XCTAssertEqual(joined.rows.count, optional ? 1 : 0)
      if optional { XCTAssertEqual(joined.rows[0][1], .null) }
    }
    try await store.close()
  }
  func testStructuredMutationsAndMalformedReferencesHaveNoEffects() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    let plan = GraphPlan(
      root: GraphOperatorID(3),
      operators: [
        .unit, .eager(input: GraphOperatorID(0)),
        .mutate(
          input: GraphOperatorID(1),
          mutations: [
            .createNode(output: GraphSlotID(0), labels: ["Doc"]),
            .createNode(output: GraphSlotID(1), labels: []),
            .createRelationship(
              output: GraphSlotID(2), source: GraphExpressionID(0), target: GraphExpressionID(1),
              type: "LINK"),
            .setProperty(entity: GraphExpressionID(0), name: "value", value: GraphExpressionID(3)),
            .setProperty(entity: GraphExpressionID(0), name: "remove", value: GraphExpressionID(3)),
            .removeProperty(entity: GraphExpressionID(0), name: "remove"),
            .setLabel(entity: GraphExpressionID(0), name: "Added", present: true),
            .setLabel(entity: GraphExpressionID(0), name: "Doc", present: false),
            .delete(entity: GraphExpressionID(2), detach: false),
          ]),
        .project(
          input: GraphOperatorID(2),
          bindings: [GraphProjection(GraphSlotID(0), GraphExpressionID(0))]),
      ],
      expressions: [
        .slot(GraphSlotID(0)), .slot(GraphSlotID(1)), .slot(GraphSlotID(2)), .literal(.integer(42)),
      ])
    var invalid = plan
    invalid.operators[3] = .project(
      input: GraphOperatorID(2), bindings: [GraphProjection(GraphSlotID(0), GraphExpressionID(99))])
    do {
      _ = try await store.graphQuery(invalid)
      XCTFail("malformed expression must reject before mutation")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .invalidArgument)
      XCTAssertNil(error.metadata?.changedGeneration)
    }
    let before = try await store.cypher("MATCH (n) RETURN n")
    XCTAssertEqual(before.rows.count, 0)
    let result = try await store.graphQuery(plan)
    XCTAssertEqual(result.metadata.disposition, .committed)
    // One store counter: enable_graph commits 1; this first graph write commits 2.
    XCTAssertEqual(result.metadata.changedGeneration, 2)
    guard case .node(let node) = result.rows[0][0] else { return XCTFail("created node") }
    XCTAssertEqual(node.properties["value"], .integer(42))
    XCTAssertNil(node.properties["remove"])
    XCTAssertEqual(node.labels, ["Added"])
    let noRelationships = try await store.cypher("MATCH ()-[r]->() RETURN r")
    XCTAssertTrue(noRelationships.rows.isEmpty)
    let deletion = try await store.graphQuery(
      GraphPlan(
        root: GraphOperatorID(2),
        operators: [
          .lookupNode(output: GraphSlotID(0), id: node.id), .eager(input: GraphOperatorID(0)),
          .mutate(
            input: GraphOperatorID(1),
            mutations: [.delete(entity: GraphExpressionID(0), detach: true)]),
        ], expressions: [.slot(GraphSlotID(0))]))
    XCTAssertEqual(deletion.metadata.disposition, .committed)
    let gone = try await store.getNodes([node.id])
    XCTAssertNil(gone.nodes[0])
    try await store.close()
    XCTAssertEqual(node.properties["value"], .integer(42))
  }
  func testRecursiveListParametersUseSharedCypherEncoding() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    let result = try await store.cypher(
      "RETURN $values",
      parameters: [
        "values": .list([.integer(1), .null, .list([.string(""), .bool(true)]), .list([])])
      ])
    XCTAssertEqual(
      result.rows,
      [
        [
          .list(
            .query,
            [.integer(1), .null, .list(.query, [.string(""), .bool(true)]), .list(.query, [])])
        ]
      ])
    var nested = GraphParameter.null
    for _ in 0..<17 { nested = .list([nested]) }
    do {
      _ = try await store.cypher("RETURN $values", parameters: ["values": nested])
      XCTFail("depth bound")
    } catch let error as GraphError {
      guard case .invalidRequest = error.reason else {
        return XCTFail("Swift list depth admission")
      }
    }
    try await store.close()
  }
  func testStructuredSearchReportsSurviveProjectionAndEligibility() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let tower = EmbeddingTower(
      modelID: "fixture", modelVersion: "1", weightsDigest: Data([1]),
      dimensions: 2, maxTokens: 10, runtime: .cpuReference, computeUnits: .cpu)
    let store = try await openGraphTestStore(at: path, mode: .create, documentTower: tower)
    var batch = GraphBatch()
    batch.node(
      key: GraphKey(namespace: "", key: "near"), revision: 1,
      .create(GraphNodeImage(labels: ["Eligible"], text: "amber", vector: [0, 0])))
    batch.node(
      key: GraphKey(namespace: "", key: "far"), revision: 1,
      .create(GraphNodeImage(text: "amber amber", vector: [3, 4])))
    let written = try await store.graphApply(batch)
    guard case .node(let near) = written.metadata.receipts[0].identity else {
      return XCTFail("identity")
    }
    for kind in [
      GraphSearchKind.vector(GraphExpressionID(0)), .text(GraphExpressionID(1)),
      .hybrid(vector: GraphExpressionID(0), text: GraphExpressionID(1)),
    ] {
      for tier in [GraphTier?.none, .some(.auto)] {
        if case .text = kind, tier != nil { continue }
        var searchOptions = GraphSearchOptions()
        if case .hybrid = kind {
          searchOptions.alpha = 0.5
          searchOptions.maxRounds = 0
        }
        var expressions: [GraphExpression] = [.literal(.integer(2)), .slot(GraphSlotID(2))]
        let activeKind: GraphSearchKind
        switch kind {
        case .vector:
          activeKind = .vector(GraphExpressionID(2))
          expressions.append(.literal(.list([.double(0), .double(0)])))
        case .text:
          activeKind = .text(GraphExpressionID(2))
          expressions.append(.literal(.string("amber")))
        case .hybrid:
          activeKind = .hybrid(vector: GraphExpressionID(2), text: GraphExpressionID(3))
          expressions.append(.literal(.list([.double(0), .double(0)])))
          expressions.append(.literal(.string("amber")))
        }
        let search = GraphSearch(
          activeKind, call: GraphSearchCallID(0), k: GraphExpressionID(0),
          node: GraphSlotID(2), score: GraphSlotID(3), tier: tier,
          eligibleSet: GraphEligibleSetID(9),
          options: searchOptions)
        let plan = GraphPlan(
          root: GraphOperatorID(3),
          operators: [
            .scanNodes(output: GraphSlotID(0), label: "Eligible"),
            .eligibleSet(
              input: GraphOperatorID(0), source: GraphSlotID(0), output: GraphEligibleSetID(9)),
            .search(GraphSearchID(0), eligibility: GraphOperatorID(1)),
            .project(
              input: GraphOperatorID(2),
              bindings: [GraphProjection(GraphSlotID(4), GraphExpressionID(1))]),
          ], expressions: expressions, searches: [search], eagerSearches: [GraphOperatorID(2)])
        let result = try await store.graphQuery(plan)
        XCTAssertEqual(result.rows.count, 1)
        guard case .node(let node) = result.rows[0][0] else { return XCTFail("search node") }
        XCTAssertEqual(node.id, near)
        XCTAssertEqual(result.columns.map(\.name), ["slot_4"])
        XCTAssertEqual(result.metadata.reports.count, 1)
        let report = result.metadata.reports[0]
        XCTAssertEqual(report.call_id, 0)
        XCTAssertEqual(report.generation, written.metadata.changedGeneration)
        XCTAssertEqual(report.has_requested_tier, tier == nil ? 0 : 1)
        if tier != nil { XCTAssertEqual(report.requested_tier, 0) }
        XCTAssertGreaterThan(report.candidate_count, 0)
      }
    }
    // An eager invocation cannot disappear because the root returns no rows.
    let invalid = GraphPlan(
      root: GraphOperatorID(2),
      operators: [
        .search(GraphSearchID(0), eligibility: nil), .unit,
        .offsetLimit(input: GraphOperatorID(1), offset: 0, limit: 0),
      ], expressions: [.literal(.string("amber")), .literal(.integer(0))],
      searches: [
        GraphSearch(
          .text(GraphExpressionID(0)), call: GraphSearchCallID(0), k: GraphExpressionID(1),
          node: GraphSlotID(0), score: GraphSlotID(1))
      ], eagerSearches: [GraphOperatorID(0)])
    do {
      _ = try await store.graphQuery(invalid)
      XCTFail("eager search with invalid k must execute under LIMIT 0")
    } catch let error as GraphError {
      XCTAssertNotNil(error.code)
      XCTAssertNil(error.metadata?.changedGeneration)
    }
    try await store.close()
  }
}

extension GraphStructuredQueryTests {
  func testStructuredReadRowsRemainOwnedWithoutNativeParameterInput() async throws {
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    var batch = GraphBatch()
    batch.node(
      key: GraphKey(namespace: "", key: "a"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["title": .string("hello")])))
    let written = try await store.graphApply(batch)
    let result = try await store.graphQuery(
      GraphPlan(
        root: GraphOperatorID(2),
        operators: [
          .scanNodes(output: GraphSlotID(0), label: nil),
          .filter(input: GraphOperatorID(0), predicate: GraphExpressionID(5)),
          .project(
            input: GraphOperatorID(1),
            bindings: [GraphProjection(GraphSlotID(7), GraphExpressionID(8))]),
        ],
        expressions: [
          .slot(GraphSlotID(0)), .property(GraphExpressionID(0), "title"),
          .literal(.string("hello")),
          .binary(.equal, GraphExpressionID(1), GraphExpressionID(2)),
          .hasLabel(GraphExpressionID(0), "Doc"),
          .binary(.and, GraphExpressionID(3), GraphExpressionID(4)),
          .literal(.list([.string(""), .list([])])), .unary(.size, GraphExpressionID(6)),
          .list([GraphExpressionID(1), GraphExpressionID(7)]),
        ]))
    try await store.close()
    XCTAssertEqual(result.rows, [[.list(.query, [.string("hello"), .integer(2)])]])
    XCTAssertEqual(result.columns.map(\.name), ["slot_7"])
    XCTAssertEqual(result.metadata.admittedGeneration, written.metadata.changedGeneration)
  }
}

#endif
