import Foundation
import Darwin
import ZeppelinEmbed

// Public Swift API measurement, including owned value conversion in cypher/apply.
@main
struct GraphWorkload {
  struct Failure: Error, CustomStringConvertible { let description: String }
  static func fullID(_ high: UInt64, _ low: UInt64) -> String {
    var hi = high, lo = low, digits = [Character]()
    repeat {
      let head = hi.quotientAndRemainder(dividingBy: 10)
      let tail = UInt64(10).dividingFullWidth((high: head.remainder, low: lo))
      digits.append(Character(String(tail.remainder)))
      hi = head.quotient; lo = tail.quotient
    } while hi != 0 || lo != 0
    return String(digits.reversed())
  }
  static func encode(_ value: GraphValue) -> [String: Any] {
    switch value {
    case .null: return ["null": true]
    case .bool(let v): return ["bool": v]
    case .integer(let v): return ["i64": v]
    case .double(let v): return ["f64_bits": String(format: "%016llx", v.bitPattern)]
    case .string(let v): return ["string": v]
    case .node(let v): return ["node": fullID(v.id.high, v.id.low)]
    case .relationship(let v): return ["relationship": fullID(v.id.high, v.id.low)]
    case .list(_, let v): return ["list": v.map(encode)]
    }
  }
  static let outputLock = NSLock()
  static func emit(_ value: [String: Any]) throws {
    outputLock.lock(); defer { outputLock.unlock() }
    let data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    FileHandle.standardOutput.write(data); FileHandle.standardOutput.write(Data([10]))
  }

  static func decodeBatch(_ path: String) throws -> GraphBatch {
    guard let records = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path))) as? [[String: Any]] else { throw Failure(description: "batch records") }
    func property(_ v: [String: Any]) throws -> GraphProperty {
      if let s = v["string"] as? String { return .string(s) }
      if let b = v["bool"] as? Bool { return .bool(b) }
      if let n = v["i64"] as? NSNumber { return .integer(n.int64Value) }
      if let b = v["f64_bits"] as? String, let bits = UInt64(b, radix: 16) { return .double(Double(bitPattern: bits)) }
      if let a = v["string_list"] as? [String] { return .strings(a) }
      throw Failure(description: "unsupported primitive property")
    }
    var batch = GraphBatch(), local = [String: GraphLocalNode]()
    func key(_ r: [String: Any]) throws -> GraphKey {
      guard let ns = r["namespace"] as? String, let value = r["key"] as? String else { throw Failure(description: "batch key") }
      return GraphKey(namespace: ns, key: value)
    }
    func token(_ k: GraphKey) -> String { k.namespace + "\u{0}" + k.key }
    for r in records {
      guard let image = r["image"] as? [String: Any], let primitiveKey = image["key"] as? [String: Any], let kind = image["kind"] as? String,
        let rawProperties = image["properties"] as? [String: [String: Any]] else { throw Failure(description: "batch image") }
      let k = try key(primitiveKey)
      let properties = try rawProperties.mapValues(property)
      if kind == "node" {
        guard let labels = image["labels"] as? [String] else { throw Failure(description: "labels") }
        let bits = image["vector_bits"] as? [NSNumber]
        let node = GraphNodeImage(labels: labels, properties: properties, text: image["text"] as? String, vector: bits?.map { Float(bitPattern: $0.uint32Value) })
        local[token(k)] = batch.node(key: k, revision: 1, .create(node))
      } else {
        func endpoint(_ name: String) throws -> GraphEndpoint {
          guard let endpoint = image[name] as? [String: Any] else { throw Failure(description: "endpoint") }
          let k = try key(endpoint)
          if let n = local[token(k)] { return .local(n) }
          guard let high = endpoint["high"] as? String, let low = endpoint["low"] as? String, let h = UInt64(high), let l = UInt64(low) else { throw Failure(description: "missing observed full endpoint ID") }
          return .node(GraphNodeID(high: h, low: l))
        }
        guard let type = image["type"] as? String else { throw Failure(description: "relationship type") }
        batch.relationship(key: k, revision: 1, .create(GraphRelationshipImage(type: type, properties: properties), try endpoint("source"), try endpoint("target")))
      }
    }
    return batch
  }

  // Decode the frozen pointer-free request into the public Swift domain API.
  // Unsupported records fail before timing; no C invocation bypasses Swift.
  static func decodePlan(_ path: String) throws -> GraphPlan {
    let lines = try String(contentsOfFile: path, encoding: .utf8).split(separator: "\n")
    guard lines.first == "ZE77JOB1" else { throw Failure(description: "wrong structured job version") }
    var records = [String: [[String]]](), bytes = [UInt8](), root: UInt32?
    for line in lines.dropFirst() {
      let fields = line.split(separator: " ").map(String.init)
      guard let tag = fields.first else { continue }
      if tag == "B" {
        guard fields.count == 2, fields[1].count % 2 == 0 else { throw Failure(description: "invalid byte pool") }
        let hex = Array(fields[1])
        for i in stride(from: 0, to: hex.count, by: 2) {
          guard let b = UInt8(String(hex[i...i+1]), radix: 16) else { throw Failure(description: "invalid hex") }
          bytes.append(b)
        }
      } else if tag == "R" {
        guard fields.count == 2, let n = UInt32(fields[1]) else { throw Failure(description: "root") }
        root = n
      } else {
        guard ["V", "N", "E", "P", "T", "S", "I", "H", "G", "O"].contains(tag) else { throw Failure(description: "unknown structured record \(tag)") }
        records[tag, default: []].append(Array(fields.dropFirst()))
      }
    }
    func number(_ row: [String], _ i: Int) throws -> UInt32 {
      guard row.indices.contains(i), let n = UInt32(row[i]) else { throw Failure(description: "structured integer") }
      return n
    }
    func wide(_ row: [String], _ i: Int) throws -> UInt64 {
      guard row.indices.contains(i), let n = UInt64(row[i]) else { throw Failure(description: "structured wide integer") }
      return n
    }
    func slice<T>(_ array: [T], _ start: UInt32, _ count: UInt32) throws -> [T] {
      let a = Int(start), b = a + Int(count)
      guard a <= array.count, b <= array.count else { throw Failure(description: "structured range") }
      return Array(array[a..<b])
    }
    func text(_ start: UInt32, _ count: UInt32) throws -> String {
      guard let value = String(bytes: try slice(bytes, start, count), encoding: .utf8) else { throw Failure(description: "structured UTF8") }
      return value
    }
    let inputs = try (records["I"] ?? []).map { GraphOperatorID(try number($0, 0)) }
    let children = try (records["H"] ?? []).map { GraphExpressionID(try number($0, 0)) }
    let names = try (records["N"] ?? []).map { try text(number($0, 0), number($0, 1)) }
    let values: [GraphParameter] = try (records["V"] ?? []).map { r in
      switch try number(r, 0) {
      case 2: guard r.count == 5, let n = Int64(r[1]) else { throw Failure(description: "literal integer") }; return .integer(n)
      case 3: return .double(Double(bitPattern: try wide(r, 2)))
      case 4: return .string(try text(number(r, 3), number(r, 4)))
      default: throw Failure(description: "unsupported literal")
      }
    }
    let expressions: [GraphExpression] = try (records["E"] ?? []).map { r in
      switch try number(r, 0) {
      case 0: let i = Int(try number(r, 6)); guard values.indices.contains(i) else { throw Failure(description: "literal index") }; return .literal(values[i])
      case 1: return .slot(GraphSlotID(try number(r, 6)))
      case 3: guard let op = GraphUnaryOperation(rawValue: try number(r, 1)) else { throw Failure(description: "unary") }; return .unary(op, GraphExpressionID(try number(r, 2)))
      case 4: guard let op = GraphBinaryOperation(rawValue: try number(r, 1)) else { throw Failure(description: "binary") }; return .binary(op, GraphExpressionID(try number(r, 2)), GraphExpressionID(try number(r, 3)))
      case 5: return .property(GraphExpressionID(try number(r, 2)), try text(number(r, 7), number(r, 8)))
      case 7: return .list(try slice(children, number(r, 9), number(r, 10)))
      default: throw Failure(description: "unsupported expression")
      }
    }
    let projections = try (records["P"] ?? []).map { GraphProjection(GraphSlotID(try number($0, 0)), GraphExpressionID(try number($0, 1))) }
    let sorts = try (records["T"] ?? []).map { GraphSortKey(GraphExpressionID(try number($0, 0)), descending: try number($0, 1) == 1) }
    let searches: [GraphSearch] = try (records["S"] ?? []).map { r in
      let kind: GraphSearchKind
      switch try number(r, 0) {
      case 0: kind = .vector(GraphExpressionID(try number(r, 3)))
      case 1: kind = .text(GraphExpressionID(try number(r, 5)))
      case 2: kind = .hybrid(vector: GraphExpressionID(try number(r, 3)), text: GraphExpressionID(try number(r, 5)))
      default: throw Failure(description: "search kind")
      }
      var options = GraphSearchOptions()
      if try number(r, 17) == 1 { guard r.count == 19, let alpha = Double(r[18]) else { throw Failure(description: "alpha") }; options.alpha = alpha }
      let tier: GraphTier?
      if try number(r, 7) == 1 { guard let t = GraphTier(rawValue: try number(r, 8)) else { throw Failure(description: "tier") }; tier = t } else { tier = nil }
      return GraphSearch(kind, call: GraphSearchCallID(try number(r, 1)), k: GraphExpressionID(try number(r, 6)), node: GraphSlotID(try number(r, 11)), score: GraphSlotID(try number(r, 12)), tier: tier,
        eligibleSet: try number(r, 9) == 1 ? GraphEligibleSetID(try number(r, 10)) : nil,
        vectorDistance: try number(r, 13) == 1 ? GraphSlotID(try number(r, 14)) : nil,
        lexicalScore: try number(r, 15) == 1 ? GraphSlotID(try number(r, 16)) : nil, options: options)
    }
    let operators: [GraphOperator] = try (records["O"] ?? []).map { r in
      let ins = try slice(inputs, number(r, 23), number(r, 24))
      func input() throws -> GraphOperatorID { guard ins.count == 1 else { throw Failure(description: "operator input arity") }; return ins[0] }
      switch try number(r, 0) {
      case 10: return .lookupNode(output: GraphSlotID(try number(r, 2)), id: GraphNodeID(high: try wide(r, 21), low: try wide(r, 22)))
      case 13, 14:
        guard let direction = GraphDirection(rawValue: try number(r, 4)) else { throw Failure(description: "direction") }
        let expansion = GraphExpansion(source: GraphSlotID(try number(r, 1)), node: GraphSlotID(try number(r, 2)), relationship: GraphSlotID(try number(r, 3)), direction: direction, types: try slice(names, number(r, 6), number(r, 7)), pattern: GraphPatternID(try number(r, 5)))
        if try number(r, 0) == 13 { return .expand(input: try input(), expansion) }
        return .boundedExpand(input: try input(), expansion, min: try number(r, 8), max: try number(r, 9), edgePredicate: nil)
      case 20: return .eligibleSet(input: try input(), source: GraphSlotID(try number(r, 1)), output: GraphEligibleSetID(try number(r, 10)))
      case 18: return .filter(input: try input(), predicate: GraphExpressionID(try number(r, 11)))
      case 3: return .sort(input: try input(), keys: try slice(sorts, number(r, 13), number(r, 14)))
      case 9: return .offsetLimit(input: try input(), offset: try wide(r, 15), limit: try number(r, 17) == 1 ? wide(r, 16) : nil)
      case 16: return .project(input: try input(), bindings: try slice(projections, number(r, 18), number(r, 19)))
      case 8: guard ins.count <= 1 else { throw Failure(description: "search input arity") }; return .search(GraphSearchID(try number(r, 20)), eligibility: ins.first)
      default: throw Failure(description: "unsupported operator")
      }
    }
    guard let root else { throw Failure(description: "missing root") }
    return GraphPlan(root: GraphOperatorID(root), operators: operators, expressions: expressions, searches: searches,
      eagerSearches: try (records["G"] ?? []).map { GraphOperatorID(try number($0, 0)) })
  }

  static func snapshot(_ result: GraphResult) -> [String: Any] {
    let receipts = result.metadata.receipts.map { r -> [String: Any] in
      let id: String, kind: String
      switch r.identity { case .node(let n): id = fullID(n.high, n.low); kind = "node"
        case .relationship(let v): id = fullID(v.high, v.low); kind = "relationship" }
      return ["item": r.item, "kind": kind, "id": id, "generation": r.generation, "revision": r.revision]
    }
    return ["rows": result.rows.map { $0.map(encode) },
      "generation": result.metadata.admittedGeneration.map { $0 as Any } ?? NSNull(),
      "changed_generation": result.metadata.changedGeneration.map { $0 as Any } ?? NSNull(),
      "receipts": receipts,
      "work_raw": result.metadata.globalWork.map { ["kind": UInt64($0.kind), "value": $0.units] },
      "reports_raw": result.metadata.reports.map { ["call_id": UInt64($0.call_id), "precision": UInt64($0.precision), "coverage": UInt64($0.coverage), "candidate_count": $0.candidate_count, "cross_scored_count": $0.cross_scored_count, "fallback_count": $0.fallback_count] }]
  }
  static func timed(_ store: ZeppelinStore, source: String, batch: GraphBatch?, plan: GraphPlan? = nil,
    sample: Int, participant: Int, scheduleIndex: Int) async throws -> [String: Any] {
    let start = DispatchTime.now().uptimeNanoseconds
    var result: GraphResult?
    if let plan { result = try await store.graphQuery(plan) } else if let batch { result = try await store.graphApply(batch) } else { result = try await store.cypher(source) }
    let elapsed = DispatchTime.now().uptimeNanoseconds - start
    guard var row = result.map(snapshot) else { throw Failure(description: "missing completed result") }
    let disposeStart = DispatchTime.now().uptimeNanoseconds
    result = nil
    row["disposal_ns"] = DispatchTime.now().uptimeNanoseconds - disposeStart
    row["elapsed_ns"] = elapsed; row["sample"] = sample; row["participant"] = participant
    row["schedule_index"] = scheduleIndex; row["status"] = 0
    let resources = try await store.graphResources()
    row["resources"] = ["engine_bytes": resources.engineBytes, "engine_peak_bytes": resources.enginePeakBytes,
      "application_bytes": resources.applicationBytes, "application_peak_bytes": resources.applicationPeakBytes]
    return row
  }
  actor Barrier {
    var waiting = [CheckedContinuation<Void, Never>]()
    func arrive() async {
      await withCheckedContinuation { continuation in
        waiting.append(continuation)
        if waiting.count == 5 { for c in waiting { c.resume() }; waiting.removeAll() }
      }
    }
  }
  static func mixed(_ path: String, _ readList: String, _ writeList: String) async throws {
    let reads = try String(contentsOfFile: readList, encoding: .utf8).split(separator: "\n").map(String.init)
    let writes = try String(contentsOfFile: writeList, encoding: .utf8).split(separator: "\n").map(String.init)
    guard reads.count == 100, writes.count == 200 else { throw Failure(description: "mixed requires 100 frozen cases and 200 distinct writes") }
    let tower = EmbeddingTower(modelID: "ze73-fixture", modelVersion: "1", weightsDigest: Data([0x73]), dimensions: 768, maxTokens: 512, runtime: .cpuReference, computeUnits: .cpu)
    let store = try await ZeppelinStore.openWithEpoch(at: URL(fileURLWithPath: path), epoch: Epoch(embedding: EmbeddingEpoch(document: tower, query: tower)), options: OpenOptions(durabilityMode: .durable, commitTier: .durable, maxResidentBytes: 256 << 20))
    _ = try await store.enableGraph()
    let barrier = Barrier()
    try await withThrowingTaskGroup(of: Void.self) { group in
      for participant in 0..<5 {
        group.addTask {
          await barrier.arrive()
          for i in 0..<(participant == 4 ? 200 : 1000) {
            let index = participant == 4 ? i : (i + participant * 17) % 100
            let plan = participant == 4 ? nil : try decodePlan(reads[index])
            let batch = participant == 4 ? try decodeBatch(writes[index]) : nil
            let row = try await timed(store, source: "", batch: batch, plan: plan, sample: i, participant: participant, scheduleIndex: index)
            try emit(row)
          }
        }
      }
      try await group.waitForAll()
    }
    try await store.close()
  }
  // Smoke observer reports real OS readings. Worker QoS is unavailable here;
  // the acceptance observer must supply it, rather than inventing a value.
  static func observe(_ pid: Int32) throws {
    func command(_ path: String, _ arguments: [String]) throws -> String {
      let process = Process(), output = Pipe()
      process.executableURL = URL(fileURLWithPath: path); process.arguments = arguments
      process.standardOutput = output
      try process.run()
      let data = output.fileHandleForReading.readDataToEndOfFile()
      process.waitUntilExit()
      guard process.terminationStatus == 0, let value = String(data: data, encoding: .utf8) else { throw Failure(description: "OS observer command failed") }
      return value
    }
    let start = DispatchTime.now().uptimeNanoseconds
    repeat {
      let power = try command("/usr/bin/pmset", ["-g", "batt"])
      var cpu = host_cpu_load_info(), count = mach_msg_type_number_t(MemoryLayout<host_cpu_load_info>.size / MemoryLayout<integer_t>.size)
      let status = withUnsafeMutablePointer(to: &cpu) { pointer in
        pointer.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
          host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, $0, &count)
        }
      }
      guard status == KERN_SUCCESS else { throw Failure(description: "host CPU observation failed") }
      let ticks = cpu.cpu_ticks
      let busy = Double(ticks.0) + Double(ticks.1) + Double(ticks.3)
      let total = busy + Double(ticks.2)
      guard total > 0 else { throw Failure(description: "host CPU ticks unavailable") }
      let thermal: String
      switch ProcessInfo.processInfo.thermalState {
      case .nominal: thermal = "nominal"
      case .fair: thermal = "fair"
      case .serious: thermal = "serious"
      case .critical: thermal = "critical"
      @unknown default: thermal = "unknown"
      }
      try emit(["elapsed_ms": (DispatchTime.now().uptimeNanoseconds - start) / 1_000_000,
        "thermal": thermal, "power": ProcessInfo.processInfo.isLowPowerModeEnabled ? "low-power" : "normal",
        "ac": power.contains("AC Power"), "qos": NSNull(),
        "host_cpu_fraction": busy / total, "cpu_method": "Mach host cumulative CPU ticks; includes worker",
        "scope": "smoke observer; worker QoS requires acceptance observer"])
      if kill(pid, 0) != 0 { break }
      Thread.sleep(forTimeInterval: 1)
    } while true
  }
  static func main() async {
    do { try await run() } catch {
      FileHandle.standardError.write(Data("ZE-77 Swift: \(error)\n".utf8))
      exit(1)
    }
  }
  static func run() async throws {
    let args = Array(CommandLine.arguments.dropFirst())
    if args.count == 2 && args[0] == "observe", let pid = Int32(args[1]) { try observe(pid); return }
    if args.count == 4 && args[0] == "mixed" { try await mixed(args[1], args[2], args[3]); return }
    guard args.count == 5, let warmups = Int(args[3]), let samples = Int(args[4]),
      (0...1000).contains(warmups), (1...10000).contains(samples) else {
      throw Failure(description: "usage: graph-workload-swift STORE structured|cypher JOB WARMUPS SAMPLES")
    }
    guard ["structured", "cypher", "batch"].contains(args[1]) else { throw Failure(description: "unknown frontend") }
    let paths: [String]
    if args[2].hasPrefix("@") { paths = try String(contentsOfFile: String(args[2].dropFirst()), encoding: .utf8).split(separator: "\n").map(String.init) }
    else { paths = [args[2]] }
    guard !paths.isEmpty else { throw Failure(description: "empty fixed schedule") }
    let tower = EmbeddingTower(modelID: "ze73-fixture", modelVersion: "1",
      weightsDigest: Data([0x73]), dimensions: 768, maxTokens: 512,
      runtime: .cpuReference, computeUnits: .cpu)
    FileHandle.standardError.write(Data("ZE-77 Swift phase: open\n".utf8))
    let store = try await ZeppelinStore.openWithEpoch(at: URL(fileURLWithPath: args[0]), epoch: Epoch(embedding: EmbeddingEpoch(document: tower, query: tower)), options: OpenOptions(durabilityMode: .durable, commitTier: .durable, maxResidentBytes: 256 << 20))
    _ = try await store.enableGraph()
    FileHandle.standardError.write(Data("ZE-77 Swift phase: opened; entering public requests\n".utf8))
    for i in 0..<(warmups + samples) {
      let job = paths[i % paths.count]
      let source = args[1] == "cypher" ? try String(contentsOfFile: job, encoding: .utf8) : ""
      let batch = args[1] == "batch" ? try decodeBatch(job) : GraphBatch()
      var row = try await timed(store, source: source, batch: args[1] == "batch" ? batch : nil,
        plan: args[1] == "structured" ? try decodePlan(job) : nil,
        sample: max(0, i - warmups), participant: 0, scheduleIndex: i % paths.count)
      row["warmup"] = i < warmups
      try emit(row)
    }
    try await store.close()
  }
}
