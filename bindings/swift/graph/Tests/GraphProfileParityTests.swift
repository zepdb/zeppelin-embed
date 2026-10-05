import Foundation
import CoreFoundation
import XCTest
import ZeppelinEmbedGraph

final class GraphProfileParityTests: XCTestCase {
  private var ignoreListOrder = false
  func testSharedPositiveCypherProfile() async throws {
    guard let manifestPath = ProcessInfo.processInfo.environment["ZE74_SWIFT_MANIFEST"] else {
      throw Failure.missingManifest
    }
    let root = URL(fileURLWithPath: manifestPath)
    let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf: root)) as! [String: Any]
    scenarios: for scenario in manifest["cases"] as! [[String: Any]] {
      if !(scenario["error"] is NSNull) { continue }
      ignoreListOrder = (scenario["mode"] as? String) == "bag-lists-unordered"
      print("ZE74 Swift case: \(scenario["id"]!)")
      let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: directory) }
      let store = try await ZeppelinGraphStore.open(at: directory, mode: .create)
      for setup in scenario["setup"] as! [String] { do { _ = try await store.cypher(setup, controls: GraphControls(rowLimit: 1024)) } catch { print("SETUP FAILURE \(error)"); throw error } }
      let before: State
      do { before = try await snapshot(store) } catch { print("SNAPSHOT FAILURE \(error)"); throw error }
      var parameters: [String: GraphParameter] = [:]
      for binding in scenario["parameters"] as! [[String]] {
        switch try parse(binding[1]) {
        case .null: parameters[binding[0]] = .null
        case .bool(let v): parameters[binding[0]] = .bool(v)
        case .integer(let v): parameters[binding[0]] = .integer(v)
        case .double(let v): parameters[binding[0]] = .double(v)
        case .string(let v): parameters[binding[0]] = .string(v)
        default:
          XCTFail("BLOCKED ZE-278: public GraphParameter list marshalling missing for \(scenario["id"]!)")
          try await store.close()
          continue scenarios
        }
      }
      let result = try await store.cypher(scenario["query"] as! String, parameters: parameters, controls: GraphControls(rowLimit: 1024))
      let after = try await snapshot(store)
      try await store.close()
      let reopened = try await ZeppelinGraphStore.open(at: directory, mode: .readWrite)
      let durable = try await snapshot(reopened)
      try await reopened.close()
      XCTAssertEqual(after, durable, "reopen: \(scenario["id"]!)")
      let header = scenario["header"] as! [String]
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
        let receipt: [String: Any] = ["case": scenario["id"]!, "path": "swift-cypher",
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
  func testSharedPublicCypherRejections() async throws {
    guard let path = ProcessInfo.processInfo.environment["ZE74_SWIFT_MANIFEST"] else { throw Failure.missingManifest }
    let manifest = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path))) as! [String: Any]
    for scenario in manifest["cases"] as! [[String: Any]] {
      guard let expected = scenario["error"] as? [String: Any] else { continue }
      let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
      defer { try? FileManager.default.removeItem(at: directory) }
      let store = try await ZeppelinGraphStore.open(at: directory, mode: .create)
      for setup in scenario["setup"] as! [String] { _ = try await store.cypher(setup, controls: GraphControls(rowLimit: 1024)) }
      let before = try await snapshot(store)
      var observedCode: Int32?
      do {
        _ = try await store.cypher(scenario["query"] as! String, controls: GraphControls(rowLimit: 1024))
        XCTFail("expected public rejection for \(scenario["id"]!)")
      } catch let error as GraphError {
        guard case .native(let code) = error.reason else { throw error }
        observedCode = code
        XCTAssertEqual(code, Int32(expected["c_code"] as! Int), "\(scenario["id"]!)")
      }
      let after = try await snapshot(store)
      XCTAssertEqual(before, after)
      try await store.close()
      let reopened = try await ZeppelinGraphStore.open(at: directory, mode: .readWrite)
      let durable = try await snapshot(reopened)
      try await reopened.close()
      XCTAssertEqual(after, durable)
      if let output = ProcessInfo.processInfo.environment["ZE74_SWIFT_RECEIPTS"] {
        let receipt: [String: Any] = ["case": scenario["id"]!, "path": "swift-cypher",
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
  private func snapshot(_ store: ZeppelinGraphStore) async throws -> State {
    var state = State(); var bytes = 0
    for (relationship, query) in [(false, "MATCH (n) RETURN ze.node_id(n), n"),
      (true, "MATCH ()-[r]->() RETURN ze.relationship_id(r), r")] {
      let result = try await store.cypher(query, controls: GraphControls(rowLimit: 1024))
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
