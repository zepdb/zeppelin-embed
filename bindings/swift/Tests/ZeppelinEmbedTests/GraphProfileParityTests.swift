#if ZE_GRAPH
import Foundation
import CoreFoundation
import XCTest
import ZeppelinEmbed

final class GraphProfileParityTests: XCTestCase {
  private var ignoreListOrder = false
  private var structured = false
  func testSharedPositiveStructuredProfile() async throws { structured = true; try await positiveProfile() }
  func testSharedPublicStructuredRejections() async throws { structured = true; try await rejections() }
  func testSharedPositiveCypherProfile() async throws { try await positiveProfile() }
  private func positiveProfile() async throws {
    guard let manifestPath = ProcessInfo.processInfo.environment["ZE74_SWIFT_MANIFEST"] else {
      throw XCTSkip("ZE-74 manifest absent: run scripts/graph-profile-parity.py run")
    }
    let root = URL(fileURLWithPath: manifestPath)
    let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf: root)) as! [String: Any]
    for scenario in manifest["cases"] as! [[String: Any]] {
      if !(scenario["error"] is NSNull) { continue }
      if let selected = ProcessInfo.processInfo.environment["ZE74_SWIFT_CASE"], selected != scenario["id"] as! String { continue }
      if structured && scenario["structured"] is NSNull { continue }
      ignoreListOrder = (scenario["mode"] as? String) == "bag-lists-unordered"
      print("ZE74 Swift case: \(scenario["id"]!)")
      let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: directory) }
      let store = try await openGraphTestStore(at: directory, mode: .create)
      for setup in scenario["setup"] as! [String] { do { _ = try await store.cypher(setup, controls: GraphControls(rowLimit: 1024)) } catch { print("SETUP FAILURE \(error)"); throw error } }
      let before: State
      do { before = try await snapshot(store) } catch { print("SNAPSHOT FAILURE \(error)"); throw error }
      var parameters: [String: GraphParameter] = [:]
      for binding in scenario["parameters"] as! [[String]] {
        parameters[binding[0]] = try parameter(parse(binding[1]))
      }
      let result: GraphResult
      if structured {
        result = try await store.graphQuery(plan(scenario["structured"] as! [String: Any]), parameters: parameters)
      } else {
        result = try await store.cypher(scenario["query"] as! String, parameters: parameters, controls: GraphControls(rowLimit: 1024))
      }
      let after = try await snapshot(store)
      try await store.close()
      let reopened = try await openGraphTestStore(at: directory, mode: .readWrite)
      let durable = try await snapshot(reopened)
      try await reopened.close()
      XCTAssertEqual(after, durable, "reopen: \(scenario["id"]!)")
      let header = structured ? (scenario["header"] as! [String]).indices.map { "slot_\(42 + $0)" } : scenario["header"] as! [String]
      if !header.isEmpty { XCTAssertEqual(result.columns.map(\.name), header) }
      if let kinds = scenario["column_kinds"] as? [Int] {
        XCTAssertEqual(result.columns.map { Int($0.kinds) }, kinds)
      }
      let expected = scenario["expected_rows"] as! [String]
      let ordered = scenario["ordered"] as! Bool
      var observed = result.rows.map { "[" + $0.map(debugValue).joined(separator: ", ") + "]" }
      if !ordered { observed.sort() }
      XCTAssertEqual(observed, expected, "declared typed rows: \(scenario["id"]!)")
      // Independent global label and property tuple snapshots preserve full IDs.
      let delta = diff(before, after)
      XCTAssertEqual(delta, scenario["effects"] as! [Int])
      let changed = (scenario["effects"] as! [Int]).contains { $0 != 0 }
      let expectedGeneration = before.generation! + (changed ? 1 : 0)
      let disposition: GraphDisposition = changed ? .committed : ((scenario["write"] as? Bool) == true ? .noOp : .notApplicable)
      XCTAssertEqual(after.generation, expectedGeneration)
      XCTAssertEqual(result.metadata.admittedGeneration, before.generation)
      XCTAssertEqual(result.metadata.changedGeneration, changed ? expectedGeneration : nil)
      XCTAssertEqual(result.metadata.disposition, disposition)
      if let output = ProcessInfo.processInfo.environment["ZE74_SWIFT_RECEIPTS"] {
        let rows = result.rows.map { "[" + $0.map(debugValue).joined(separator: ", ") + "]" }.sorted()
        let receipt: [String: Any] = ["case": scenario["id"]!, "path": structured ? "swift-structured" : "swift-cypher",
          "state": observed == expected && delta == (scenario["effects"] as! [Int]) &&
            after == durable && after.generation == expectedGeneration && result.metadata.disposition == disposition ? "focused GREEN" : "failed",
          "columns": result.columns.map(\.name), "column_kinds": result.columns.map { Int($0.kinds) }, "rows": rows,
          "expected_rows": expected,
          "effects": delta, "reopened": true, "released": true]
        let data = try JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys]) + Data([10])
        if !FileManager.default.fileExists(atPath: output) { FileManager.default.createFile(atPath: output, contents: nil) }
        let file = try FileHandle(forWritingTo: URL(fileURLWithPath: output))
        try file.seekToEnd(); try file.write(contentsOf: data); try file.close()
      }
    }
  }
  func testSharedPublicCypherRejections() async throws { try await rejections() }
  private func rejections() async throws {
    guard let path = ProcessInfo.processInfo.environment["ZE74_SWIFT_MANIFEST"] else { throw XCTSkip("ZE-74 manifest absent: run scripts/graph-profile-parity.py run") }
    let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path))) as! [String: Any]
    for scenario in manifest["cases"] as! [[String: Any]] {
      guard let expected = scenario["error"] as? [String: Any] else { continue }
      if let selected = ProcessInfo.processInfo.environment["ZE74_SWIFT_CASE"], selected != scenario["id"] as! String { continue }
      if structured && scenario["structured"] is NSNull { continue }
      let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: directory) }
      let store = try await openGraphTestStore(at: directory, mode: .create)
      for setup in scenario["setup"] as! [String] { _ = try await store.cypher(setup, controls: GraphControls(rowLimit: 1024)) }
      let before = try await snapshot(store)
      var observedCode: Int32?
      do {
        if structured { _ = try await store.graphQuery(plan(scenario["structured"] as! [String: Any])) }
        else { _ = try await store.cypher(scenario["query"] as! String, controls: GraphControls(rowLimit: 1024)) }
        XCTFail("expected public rejection for \(scenario["id"]!)")
      } catch let error as GraphError {
        guard case .native(let code) = error.reason else { throw error }
        observedCode = code
        XCTAssertEqual(code, Int32(expected["c_code"] as! Int), "\(scenario["id"]!)")
      }
      let after = try await snapshot(store)
      XCTAssertEqual(before, after)
      try await store.close()
      let reopened = try await openGraphTestStore(at: directory, mode: .readWrite)
      let durable = try await snapshot(reopened)
      try await reopened.close()
      XCTAssertEqual(after, durable)
      if let output = ProcessInfo.processInfo.environment["ZE74_SWIFT_RECEIPTS"] {
        let receipt: [String: Any] = ["case": scenario["id"]!, "path": structured ? "swift-structured" : "swift-cypher",
          "state": before == after && after == durable && observedCode == Int32(expected["c_code"] as! Int) ? "focused GREEN" : "failed",
          "error": String(observedCode ?? -1), "error_code": observedCode ?? -1,
          "effects": [0,0,0,0,0,0,0,0], "reopened": true, "released": true]
        let data = try JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys]) + Data([10])
        if !FileManager.default.fileExists(atPath: output) { FileManager.default.createFile(atPath: output, contents: nil) }
        let file = try FileHandle(forWritingTo: URL(fileURLWithPath: output))
        try file.seekToEnd(); try file.write(contentsOf: data); try file.close()
      }
    }
  }
  private func parameter(_ value: GraphValue) throws -> GraphParameter {
    switch value {
    case .null: return .null
    case .bool(let v): return .bool(v)
    case .integer(let v): return .integer(v)
    case .double(let v): return .double(v)
    case .string(let v): return .string(v)
    case .list(_, let v): return .list(try v.map(parameter))
    default: throw Failure.identity
    }
  }
  // Translate the independently authored fixture arenas to the public typed API.
  private func plan(_ data: [String: Any]) throws -> GraphPlan {
    func number(_ row: [String: Any], _ key: String, _ fallback: UInt32 = 0) -> UInt32 { (row[key] as? NSNumber)?.uint32Value ?? fallback }
    func ids(_ row: [String: Any], _ key: String) -> [Int] { row[key] as? [Int] ?? [] }
    func expressionID(_ row: [String: Any], _ key: String) -> GraphExpressionID { GraphExpressionID(number(row, key)) }
    let expressions: [GraphExpression] = try (data["expressions"] as! [[String: Any]]).map { e in
      switch e["kind"] as! String {
      case "literal": return .literal(try parameter(value(e["value"]!)))
      case "slot": return .slot(GraphSlotID(number(e, "value")))
      case "parameter": return .parameter(GraphParameterID(number(e, "value")))
      case "list": return .list(ids(e, "items").map { GraphExpressionID(UInt32($0)) })
      case "property": return .property(expressionID(e, "left"), e["name"] as! String)
      case "label": return .hasLabel(expressionID(e, "left"), e["name"] as! String)
      case "unary": return .unary(GraphUnaryOperation(rawValue: number(e, "operation"))!, expressionID(e, "left"))
      case "binary": return .binary(GraphBinaryOperation(rawValue: number(e, "operation"))!, expressionID(e, "left"), expressionID(e, "right"))
      case "count", "collect": return .aggregate(e["kind"] as! String == "count" ? .count : .collect, operand: (e["operand"] as? NSNumber).map { GraphExpressionID($0.uint32Value) }, distinct: e["distinct"] as? Bool ?? false)
      default: throw Failure.identity
      }
    }
    let projections = (data["projections"] as! [[String: Any]]).map { GraphProjection(GraphSlotID(number($0, "slot")), expressionID($0, "expression")) }
    let mutations: [GraphMutation] = try (data["mutations"] as? [[String: Any]] ?? [["kind": "create", "output": 3, "labels": [String]()]]).map { m in
      switch m["kind"] as! String {
      case "create": return .createNode(output: GraphSlotID(number(m, "output")), labels: m["labels"] as! [String])
      case "remove": return .removeProperty(entity: expressionID(m, "entity"), name: m["name"] as! String)
      case "label": return .setLabel(entity: expressionID(m, "entity"), name: m["name"] as! String, present: m["present"] as! Bool)
      case "delete": return .delete(entity: expressionID(m, "entity"), detach: m["detach"] as! Bool)
      case "set": return .setProperty(entity: expressionID(m, "entity"), name: m["name"] as! String, value: expressionID(m, "value"))
      default: throw Failure.identity
      }
    }
    // C/Swift sources have zero arity. Remove only arenas made unreachable by
    // that ABI difference, retaining explicit Unit inputs to OptionalApply.
    var rows = data["operators"] as! [[String: Any]]
    for i in rows.indices where rows[i]["kind"] as! String == "scan" { rows[i]["inputs"] = [Int]() }
    var reachable = Set<Int>()
    func visit(_ i: Int) { if reachable.insert(i).inserted { for child in ids(rows[i], "inputs") { visit(child) } } }
    visit(Int(number(data, "root")))
    let kept = reachable.sorted()
    let remap = Dictionary(uniqueKeysWithValues: kept.enumerated().map { ($0.element, UInt32($0.offset)) })
    let operators: [GraphOperator] = try kept.map { i in
      let o = rows[i], inputs = ids(rows[i], "inputs").map { GraphOperatorID(remap[$0]!) }
      let predicate = (o["predicate"] as? NSNumber).map { GraphExpressionID($0.uint32Value) }
      func bindings(_ key: String) -> [GraphProjection] { ids(o, key).map { projections[$0] } }
      switch o["kind"] as! String {
      case "unit": return .unit
      case "scan": return .scanNodes(output: GraphSlotID(number(o, "output")), label: o["label"] as? String)
      case "join": return .join(left: inputs[0], right: inputs[1], predicate: predicate)
      case "distinct": return .distinct(input: inputs[0])
      case "filter": return .filter(input: inputs[0], predicate: predicate!)
      case "project": return .project(input: inputs[0], bindings: bindings("projections"))
      case "with": return .with(input: inputs[0], bindings: bindings("projections"))
      case "aggregate": return .aggregate(input: inputs[0], keys: bindings("projections"), aggregates: bindings("aggregates"))
      case "eager": return .eager(input: inputs[0])
      case "mutate": return .mutate(input: inputs[0], mutations: o["mutations"] == nil ? mutations : ids(o, "mutations").map { mutations[$0] })
      case "limit": return .offsetLimit(input: inputs[0], offset: 0, limit: (o["limit"] as! NSNumber).uint64Value)
      case "optional": return .optionalApply(left: inputs[0], right: inputs[1], predicate: predicate)
      case "expand", "bounded":
        let expansion = GraphExpansion(source: GraphSlotID(number(o, "source")), node: GraphSlotID(number(o, "node", 2)), relationship: GraphSlotID(number(o, "relationship", 1)), direction: o["direction"] as? String == "either" ? .either : .outgoing, types: o["types"] as? [String] ?? [], pattern: GraphPatternID(number(o, "pattern")))
        if o["kind"] as! String == "expand" { return .expand(input: inputs[0], expansion) }
        return .boundedExpand(input: inputs[0], expansion, min: number(o, "min"), max: number(o, "max"), edgePredicate: nil)
      default: throw Failure.identity
      }
    }
    return GraphPlan(root: GraphOperatorID(remap[Int(number(data, "root"))]!), operators: operators, expressions: expressions, parameters: (data["parameters"] as? [String] ?? []).map { GraphParameterDeclaration($0, kinds: .nonentity) })
  }
  private func parse(_ text: String) throws -> GraphValue {
    let data = Data(text.replacingOccurrences(of: "'", with: "\"").utf8)
    return value(try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]))
  }
  private func value(_ object: Any) -> GraphValue {
    if object is NSNull { return .null }
    if let s = object as? String { return .string(s) }
    if let list = object as? [Any] { return .list(.query, list.map(value)) }
    let number = object as! NSNumber
    if CFGetTypeID(number) == CFBooleanGetTypeID() { return .bool(number.boolValue) }
    if String(cString: number.objCType) == "d" { return .double(number.doubleValue) }
    return .integer(number.int64Value)
  }
  private struct State: Equatable {
    var generation: UInt64?
    var nodes = Set<String>(); var relationships = Set<String>(); var labels = Set<String>()
    var properties = Set<String>()
  }
  private func snapshot(_ store: ZeppelinStore) async throws -> State {
    var state = State(); var bytes = 0
    for (relationship, query) in [(false, "MATCH (n) RETURN ze.node_id(n), n"),
      (true, "MATCH ()-[r]->() RETURN ze.relationship_id(r), r")] {
      let result: GraphResult
      if structured {
        var operators: [GraphOperator] = [.scanNodes(output: GraphSlotID(0), label: nil)]
        if relationship { operators.append(.expand(input: GraphOperatorID(0), GraphExpansion(source: GraphSlotID(0), node: GraphSlotID(2), relationship: GraphSlotID(1)))) }
        let slot = GraphExpression.slot(GraphSlotID(relationship ? 1 : 0))
        let expressions: [GraphExpression] = [slot, .unary(relationship ? .relationshipIDText : .nodeIDText, GraphExpressionID(0))]
        operators.append(.project(input: GraphOperatorID(UInt32(operators.count - 1)), bindings: [GraphProjection(GraphSlotID(42), GraphExpressionID(1)), GraphProjection(GraphSlotID(43), GraphExpressionID(0))]))
        result = try await store.graphQuery(GraphPlan(root: GraphOperatorID(UInt32(operators.count - 1)), operators: operators, expressions: expressions))
      } else { result = try await store.cypher(query, controls: GraphControls(rowLimit: 1024)) }
      guard result.metadata.disposition == .notApplicable, let generation = result.metadata.admittedGeneration else { throw Failure.identity }
      if let previous = state.generation, previous != generation { throw Failure.identity }
      state.generation = generation
      for row in result.rows {
        bytes += row.map(debugValue).joined().utf8.count
        guard bytes <= 24 << 20 else { throw Failure.snapshotCap }
        guard case .string(let id) = row[0] else { throw Failure.identity }
        if relationship {
          guard case .relationship(let r) = row[1] else { throw Failure.identity }
          state.relationships.insert(id)
          for (key, v) in r.properties { state.properties.insert("r/\(id)/\(key)/\(debugValue(v))") }
        } else {
          guard case .node(let n) = row[1] else { throw Failure.identity }
          state.nodes.insert(id); state.labels.formUnion(n.labels)
          for (key, v) in n.properties { state.properties.insert("n/\(id)/\(key)/\(debugValue(v))") }
        }
      }
    }
    return state
  }
  private func diff(_ a: State, _ b: State) -> [Int] {
    [b.nodes.subtracting(a.nodes).count, a.nodes.subtracting(b.nodes).count,
     b.relationships.subtracting(a.relationships).count, a.relationships.subtracting(b.relationships).count,
     b.labels.subtracting(a.labels).count, a.labels.subtracting(b.labels).count,
     b.properties.subtracting(a.properties).count, a.properties.subtracting(b.properties).count]
  }
  private func debugValue(_ v: GraphValue) -> String {
    switch v {
    case .null: return "Null"
    case .bool(let b): return "Bool(\(b))"
    case .integer(let i): return "Int(\(i))"
    case .double(let d): return "Float(\(d))"
    case .string(let s): return "Str(\(String(data: try! JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed]), encoding: .utf8)!))"
    case .list(_, let list):
      var values = list.map(debugValue)
      if ignoreListOrder { values.sort() }
      return "List([" + values.joined(separator: ", ") + "])"
    case .node(let n): return "Node(\(n.labels.sorted()), \(properties(n.properties)))"
    case .relationship(let r): return "Rel(\(quote(r.type)), \(properties(r.properties)))"
    }
  }
  private func quote(_ s: String) -> String {
    String(data: try! JSONSerialization.data(withJSONObject: s, options: [.fragmentsAllowed, .withoutEscapingSlashes]), encoding: .utf8)!
  }
  private func properties(_ p: [String: GraphValue]) -> String {
    "{" + p.keys.sorted().map { quote($0) + ": " + debugValue(p[$0]!) }.joined(separator: ", ") + "}"
  }
  private enum Failure: Error { case snapshotCap, identity, missingManifest, missingListParameter }

}

#endif
