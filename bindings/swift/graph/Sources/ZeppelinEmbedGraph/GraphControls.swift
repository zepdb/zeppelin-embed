import CZeppelinEmbedGraph
import Foundation

public final class GraphCancellationToken: @unchecked Sendable {
  let value: UInt64
  public init() throws {
    var token: UInt64 = 0
    let code = ze_cancel_token_create(&token)
    guard code == 0 else { throw GraphError(.native(Int32(code))) }
    value = token
  }
  public func cancel() throws {
    let code = ze_cancel_token_cancel(value)
    guard code == 0 else { throw GraphError(.native(Int32(code))) }
  }
  // No public free: requests retain this token until native completion.
  deinit { _ = ze_cancel_token_free(value) }
}
public enum GraphInterruption: Sendable {
  case none
  case cancellation(GraphCancellationToken)
  case deadlineNanoseconds(UInt64)
}
public struct GraphCompileLimits: Sendable {
  public var textBytes: UInt32 = 65536, tokens: UInt32 = 8192, astNodes: UInt32 = 4096,
    depth: UInt32 = 64
  public var parameters: UInt32 = 256, columns: UInt32 = 256, listDepth: UInt32 = 16,
    pathHops: UInt32 = 16
  public init() {}
  var native: ZeGraphCompileLimits {
    var limits = ZeGraphCompileLimits()
    limits.abi_size = graphSize(ZeGraphCompileLimits.self)
    limits.text_bytes = textBytes
    limits.tokens = tokens
    limits.ast_nodes = astNodes
    limits.depth = depth
    limits.parameters = parameters
    limits.columns = columns
    limits.list_depth = listDepth
    limits.path_hops = pathHops
    return limits
  }
}
public struct GraphControls: Sendable {
  public var rowLimit: UInt32
  public var compileLimits: GraphCompileLimits?
  public var interruption: GraphInterruption
  public init(
    rowLimit: UInt32 = 65536, compileLimits: GraphCompileLimits? = nil,
    interruption: GraphInterruption = .none
  ) {
    self.rowLimit = rowLimit
    self.compileLimits = compileLimits
    self.interruption = interruption
  }
  func withControl<T>(_ body: (UnsafePointer<ZeGraphControl>) throws -> T) rethrows -> T {
    var control = ZeGraphControl()
    control.abi_size = graphSize(ZeGraphControl.self)
    switch interruption {
    case .none: break
    case .cancellation(let token): control.cancel_token = token.value
    case .deadlineNanoseconds(let ns): control.deadline_ns = ns
    }
    return try withExtendedLifetime(interruption) { try withUnsafePointer(to: &control, body) }
  }
}

// Borrow the persisted document interpretation only while open runs. This does
// not load a model or introduce an embedding runtime dependency.
func withGraphTower<T>(
  _ tower: EmbeddingTower?, _ body: (UnsafePointer<ZeEmbeddingTower>?) throws -> T
) rethrows -> T {
  guard let tower else { return try body(nil) }
  let fields = [
    Array(tower.modelID.utf8), Array(tower.modelVersion.utf8), Array(tower.weightsDigest),
    Array(tower.promptPrefix.utf8), Array((tower.operatingSystemBuild ?? "").utf8),
  ]
  var offsets: [Int] = []
  var bytes: [UInt8] = []
  for field in fields {
    offsets.append(bytes.count)
    bytes.append(contentsOf: field)
  }
  return try bytes.withUnsafeBufferPointer { bytes in
    var native = ZeEmbeddingTower()
    native.model_id = bytes.baseAddress?.advanced(by: offsets[0])
    native.model_id_len = fields[0].count
    native.model_version = bytes.baseAddress?.advanced(by: offsets[1])
    native.model_version_len = fields[1].count
    native.weights_digest = bytes.baseAddress?.advanced(by: offsets[2])
    native.weights_digest_len = fields[2].count
    native.prompt_prefix = bytes.baseAddress?.advanced(by: offsets[3])
    native.prompt_prefix_len = fields[3].count
    native.os_build = bytes.baseAddress?.advanced(by: offsets[4])
    native.os_build_len = fields[4].count
    native.has_os_build = tower.operatingSystemBuild == nil ? 0 : 1
    native.dims = tower.dimensions
    native.normalization = tower.normalization.rawValue
    native.max_tokens = tower.maxTokens
    native.runtime = tower.runtime.rawValue
    native.compute_units = tower.computeUnits.rawValue
    return try withUnsafePointer(to: &native, body)
  }
}
