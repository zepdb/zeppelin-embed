import Foundation
import ZeppelinEmbedGraph

@main
struct InstalledConsumer {
  static func main() async throws {
    if CommandLine.arguments.count > 1 && CommandLine.arguments[1] == "--measure" {
      throw NSError(domain: "ZE-74", code: 1, userInfo: [NSLocalizedDescriptionKey:
        "BLOCKED: ZE-76 qualified counters, ZE-278 structured Swift API and ZE-290 sustained ingestion are missing; no timing qualification"])
    }
    if CommandLine.arguments.count > 1 && CommandLine.arguments[1] == "--profile" {
      try await profile(); return
    }
    let path = URL(fileURLWithPath: CommandLine.arguments[1])
    let store = try await ZeppelinGraphStore.open(at: path, mode: .create)
    var batch = GraphBatch()
    let a = batch.node(key: GraphKey(namespace: "fixture", key: "a"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"], properties: ["title": .string("alpha")])))
    let b = batch.node(key: GraphKey(namespace: "fixture", key: "b"), revision: 1,
      .create(GraphNodeImage(labels: ["Doc"])))
    batch.relationship(key: GraphKey(namespace: "fixture", key: "r"), revision: 1,
      .create(GraphRelationshipImage(type: "LINK"), .local(a), .local(b)))
    let written = try await store.apply(batch)
    // One store counter: enable_graph commits 1; the first graph write and its reads use 2.
    guard written.metadata.changedGeneration == 2, written.metadata.receipts.count == 3 else {
      throw Failure.invalidResult
    }
    let query = "MATCH (n:Doc)-[:LINK]->(m) WHERE n.title = $title RETURN n.title, $number"
    let result = try await store.cypher(query,
      parameters: ["title": .string("alpha"), "number": .integer(42)])
    guard result.rows == [[.string("alpha"), .integer(42)]],
      result.metadata.admittedGeneration == 2 else { throw Failure.invalidResult }
    try await store.close()
    let reopened = try await ZeppelinGraphStore.open(at: path, mode: .readWrite)
    let durable = try await reopened.cypher(query,
      parameters: ["title": .string("alpha"), "number": .integer(42)])
    guard durable.rows == result.rows, durable.metadata.admittedGeneration == 2 else {
      throw Failure.invalidResult
    }
    let values = try await reopened.cypher("RETURN null, '', [], [1,null,['nested']]")
    guard values.rows == [[.null, .string(""), .list(.query, []),
      .list(.query, [.integer(1), .null, .list(.query, [.string("nested")])])]] else {
      throw Failure.invalidResult
    }
    let bag = try await reopened.cypher("MATCH (n) RETURN null ORDER BY n")
    guard bag.rows == [[.null], [.null]] else { throw Failure.invalidResult }
    let empty = try await reopened.cypher("MATCH (n:Missing) RETURN n")
    guard empty.rows.isEmpty else { throw Failure.invalidResult }
    let structured = try await reopened.query(GraphPlan(
      root: GraphOperatorID(0),
      operators: [.scanNodes(output: GraphSlotID(7), label: nil)]))
    guard structured.rows.count == 2,
      structured.metadata.admittedGeneration == 2,
      case .node(let node) = structured.rows[0][0] else { throw Failure.invalidResult }
    let fetched = try await reopened.getNodes([node.id, node.id])
    guard fetched.nodes.count == 2, fetched.nodes[0]?.id == node.id,
      fetched.nodes[1]?.id == node.id else { throw Failure.invalidResult }
    let resources = try await reopened.resources()
    guard resources.enginePeakBytes >= resources.engineBytes else { throw Failure.invalidResult }
    try await reopened.close()
    guard values.rows[0][3] == .list(.query, [.integer(1), .null, .list(.query, [.string("nested")])]) else { throw Failure.invalidResult }
    print("ZE_GRAPH_INSTALLED_RECEIPT\t{\"executed\":[\"batch\",\"structured\",\"get\",\"cypher\"],\"resources\":true,\"artifact_kind\":\"graph-cypher\",\"exit_status\":0}")
    print("installed typed Swift batch/Cypher/reopen/null/list/bag/retained PASS")
  }
  static func primitive(_ value: GraphValue) -> [String: Any] {
    func properties(_ values: [String: GraphValue]) -> [String: Any] {
      values.mapValues(primitive)
    }
    switch value {
    case .null: return ["type": 0, "value": NSNull()]
    case .bool(let v): return ["type": 1, "value": v]
    case .integer(let v): return ["type": 2, "value": v]
    case .double(let v): return ["type": 3, "value": v]
    case .string(let v): return ["type": 4, "value": v]
    case .list(_, let v): return ["type": 7, "value": v.map(primitive)]
    case .node(let v): return ["type": 5, "value": ["labels": v.labels.sorted(), "properties": properties(v.properties)]]
    case .relationship(let v): return ["type": 6, "value": ["kind": v.type, "properties": properties(v.properties)]]
    }
  }
  static func profile() async throws {
    guard CommandLine.arguments.count == 4 else { throw Failure.invalidResult }
    let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf:
      URL(fileURLWithPath: CommandLine.arguments[2]))) as! [String: Any]
    let base = URL(fileURLWithPath: CommandLine.arguments[3])
    for (index, scenario) in (manifest["cases"] as! [[String: Any]]).enumerated() {
      let store = try await ZeppelinGraphStore.open(at: base.appendingPathComponent("case-\(index)"), mode: .create)
      for setup in scenario["setup"] as! [String] {
        _ = try await store.cypher(setup, controls: GraphControls(rowLimit: 1024))
      }
      var parameters: [String: GraphParameter] = [:]
      for (name, parameter) in scenario["parameter_cells"] as! [String: [String: Any]] {
        switch parameter["type"] as! Int {
        case 0: parameters[name] = .null
        case 1: parameters[name] = .bool(parameter["value"] as! Bool)
        case 2: parameters[name] = .integer((parameter["value"] as! NSNumber).int64Value)
        case 3: parameters[name] = .double((parameter["value"] as! NSNumber).doubleValue)
        case 4: parameters[name] = .string(parameter["value"] as! String)
        default:
          try await store.close()
          throw NSError(domain: "ZE-74", code: 1, userInfo: [NSLocalizedDescriptionKey:
            "BLOCKED ZE-278: public GraphParameter list marshalling missing for \(scenario["id"]!)"])
        }
      }
      var receipt: [String: Any] = ["case": scenario["id"]!, "consumer": "installed-swift"]
      do {
        let result = try await store.cypher(scenario["query"] as! String, parameters: parameters, controls: GraphControls(rowLimit: 1024))
        guard scenario["error"] is NSNull else { throw Failure.invalidResult }
        receipt["rows"] = result.rows.map { $0.map(primitive) }
        receipt["columns"] = result.columns.map(\.name)
        receipt["admitted_generation"] = result.metadata.admittedGeneration
        receipt["changed_generation"] = result.metadata.changedGeneration
      } catch let error as GraphError {
        guard let expected = scenario["error"] as? [String: Any],
          case .native(let code) = error.reason, code == Int32(expected["c_code"] as! Int)
        else { throw error }
        receipt["error_code"] = code
      }
      try await store.close()
      let data = try JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys])
      print(String(data: data, encoding: .utf8)!)
    }
  }
  enum Failure: Error { case invalidResult }
}
