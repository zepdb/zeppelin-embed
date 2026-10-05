import Foundation
import ZeppelinEmbedGraph

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
  static func timed(_ store: ZeppelinGraphStore, source: String, batch: GraphBatch?,
    sample: Int, participant: Int, scheduleIndex: Int) async throws -> [String: Any] {
    let start = DispatchTime.now().uptimeNanoseconds
    var result: GraphResult?
    if let batch { result = try await store.apply(batch) } else { result = try await store.cypher(source) }
    let elapsed = DispatchTime.now().uptimeNanoseconds - start
    guard var row = result.map(snapshot) else { throw Failure(description: "missing completed result") }
    let disposeStart = DispatchTime.now().uptimeNanoseconds
    result = nil
    row["disposal_ns"] = DispatchTime.now().uptimeNanoseconds - disposeStart
    row["elapsed_ns"] = elapsed; row["sample"] = sample; row["participant"] = participant
    row["schedule_index"] = scheduleIndex; row["status"] = 0
    row["missing_input"] = "ZE-76 complete resource/work reporting"
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
    let store = try await ZeppelinGraphStore.open(at: URL(fileURLWithPath: path), mode: .readWrite, documentTower: tower)
    let barrier = Barrier()
    try await withThrowingTaskGroup(of: Void.self) { group in
      for participant in 0..<5 {
        group.addTask {
          await barrier.arrive()
          for i in 0..<(participant == 4 ? 200 : 1000) {
            let index = participant == 4 ? i : (i + participant * 17) % 100
            let source = participant == 4 ? "" : try String(contentsOfFile: reads[index], encoding: .utf8)
            let batch = participant == 4 ? try decodeBatch(writes[index]) : nil
            let row = try await timed(store, source: source, batch: batch, sample: i, participant: participant, scheduleIndex: index)
            try emit(row)
          }
        }
      }
      try await group.waitForAll()
    }
    try await store.close()
  }
  static func main() async {
    do { try await run() } catch {
      FileHandle.standardError.write(Data("ZE-77 Swift: \(error)\n".utf8))
      exit(1)
    }
  }
  static func run() async throws {
    let args = Array(CommandLine.arguments.dropFirst())
    if args.count == 4 && args[0] == "mixed" { try await mixed(args[1], args[2], args[3]); return }
    guard args.count == 5, let warmups = Int(args[3]), let samples = Int(args[4]),
      (0...1000).contains(warmups), (1...10000).contains(samples) else {
      throw Failure(description: "usage: graph-workload-swift STORE structured|cypher JOB WARMUPS SAMPLES")
    }
    if args[1] == "structured" {
      throw Failure(description: "missing ZE-278: typed Swift structured-query construction/encoding and ZeppelinGraphStore.query; raw C cannot qualify Swift")
    }
    guard ["cypher", "batch"].contains(args[1]) else { throw Failure(description: "unknown frontend") }
    let paths: [String]
    if args[2].hasPrefix("@") { paths = try String(contentsOfFile: String(args[2].dropFirst()), encoding: .utf8).split(separator: "\n").map(String.init) }
    else { paths = [args[2]] }
    guard !paths.isEmpty else { throw Failure(description: "empty fixed schedule") }
    let tower = EmbeddingTower(modelID: "ze73-fixture", modelVersion: "1",
      weightsDigest: Data([0x73]), dimensions: 768, maxTokens: 512,
      runtime: .cpuReference, computeUnits: .cpu)
    FileHandle.standardError.write(Data("ZE-77 Swift phase: open\n".utf8))
    let store = try await ZeppelinGraphStore.open(at: URL(fileURLWithPath: args[0]),
      mode: .readWrite, documentTower: tower)
    FileHandle.standardError.write(Data("ZE-77 Swift phase: opened; entering public requests\n".utf8))
    for i in 0..<(warmups + samples) {
      let job = paths[i % paths.count]
      let source = args[1] == "cypher" ? try String(contentsOfFile: job, encoding: .utf8) : ""
      let batch = args[1] == "batch" ? try decodeBatch(job) : GraphBatch()
      var row = try await timed(store, source: source, batch: args[1] == "batch" ? batch : nil,
        sample: max(0, i - warmups), participant: 0, scheduleIndex: i % paths.count)
      row["warmup"] = i < warmups
      try emit(row)
    }
    try await store.close()
  }
}
