import Foundation

public struct GraphNodeID: Sendable, Equatable, Hashable {
  public let high: UInt64
  public let low: UInt64
  public init(high: UInt64, low: UInt64) {
    self.high = high
    self.low = low
  }
}
public struct GraphRelationshipID: Sendable, Equatable, Hashable {
  public let high: UInt64
  public let low: UInt64
  public init(high: UInt64, low: UInt64) {
    self.high = high
    self.low = low
  }
}
public struct GraphKey: Sendable, Equatable {
  public let namespace: String
  public let key: String
  public init(namespace: String, key: String) {
    self.namespace = namespace
    self.key = key
  }
}
public enum GraphProperty: Sendable, Equatable {
  case bool(Bool)
  case integer(Int64)
  case double(Double)
  case string(String)
  case bools([Bool])
  case integers([Int64])
  case doubles([Double])
  case strings([String])
  case emptyList
}
public enum GraphParameter: Sendable {
  case null
  case bool(Bool)
  case integer(Int64)
  case double(Double)
  case string(String)
}
public enum GraphListKind: UInt32, Sendable { case query, bool, integer, double, string, empty }
public indirect enum GraphValue: Sendable, Equatable {
  case null
  case bool(Bool)
  case integer(Int64)
  case double(Double)
  case string(String)
  case node(GraphNode)
  case relationship(GraphRelationship)
  case list(GraphListKind, [GraphValue])
}
public struct GraphNodeImage: Sendable {
  public var labels: [String]
  public var properties: [String: GraphProperty]
  public var text: String?
  public var vector: [Float]?
  public init(
    labels: [String] = [], properties: [String: GraphProperty] = [:], text: String? = nil,
    vector: [Float]? = nil
  ) {
    self.labels = labels
    self.properties = properties
    self.text = text
    self.vector = vector
  }
}
public struct GraphRelationshipImage: Sendable {
  public var type: String
  public var properties: [String: GraphProperty]
  public init(type: String, properties: [String: GraphProperty] = [:]) {
    self.type = type
    self.properties = properties
  }
}
public struct GraphNode: Sendable, Equatable {
  public let id: GraphNodeID
  public let key: GraphKey?
  public let revision: UInt64
  public let lastChangeGeneration: UInt64
  public let labels: [String]
  public let properties: [String: GraphValue]
  public let text: String?
  public let vector: [Float]?
}
public struct GraphRelationship: Sendable, Equatable {
  public let id: GraphRelationshipID
  public let source: GraphNodeID
  public let target: GraphNodeID
  public let key: GraphKey?
  public let revision: UInt64
  public let lastChangeGeneration: UInt64
  public let type: String
  public let properties: [String: GraphValue]
}
public enum GraphEndpoint: Sendable {
  case node(GraphNodeID)
  case local(GraphLocalNode)
}
public struct GraphLocalNode: Sendable {
  let batch: UUID
  let item: UInt32
}
public enum GraphNodeMutation: Sendable {
  case create(GraphNodeImage)
  case put(GraphNodeID, GraphNodeImage)
  case delete(GraphNodeID, detach: Bool)
  case recreate(deletionRevision: UInt64, GraphNodeImage)
}
public enum GraphRelationshipMutation: Sendable {
  case create(GraphRelationshipImage, GraphEndpoint, GraphEndpoint)
  case put(GraphRelationshipID, GraphRelationshipImage, GraphEndpoint, GraphEndpoint)
  case delete(GraphRelationshipID)
  case recreate(deletionRevision: UInt64, GraphRelationshipImage, GraphEndpoint, GraphEndpoint)
}
public struct GraphBatch: Sendable {
  let identity = UUID()
  enum Item: Sendable {
    case node(GraphKey, UInt64, GraphNodeMutation)
    case relationship(GraphKey, UInt64, GraphRelationshipMutation)
  }
  var items: [Item] = []
  public init() {}
  @discardableResult public mutating func node(
    key: GraphKey, revision: UInt64, _ mutation: GraphNodeMutation
  ) -> GraphLocalNode {
    let local = GraphLocalNode(batch: identity, item: UInt32(items.count))
    items.append(.node(key, revision, mutation))
    return local
  }
  public mutating func relationship(
    key: GraphKey, revision: UInt64, _ mutation: GraphRelationshipMutation
  ) { items.append(.relationship(key, revision, mutation)) }
}
public enum GraphDisposition: UInt32, Sendable {
  case notApplicable, notCommitted, committed, replayed, noOp, indeterminate
}
public enum GraphReceiptIdentity: Sendable, Equatable {
  case node(GraphNodeID)
  case relationship(GraphRelationshipID)
}
public struct GraphReceipt: Sendable {
  public let item: UInt32
  public let identity: GraphReceiptIdentity
  public let disposition: GraphDisposition
  public let deleted: Bool
  public let revision: UInt64
  public let generation: UInt64
}
public struct GraphDiagnostic: Sendable {
  public let code: Int32
  public let operatorIndex: UInt32?
  public let sourceSpan: Range<Int>?
  public let message: String
}
public struct GraphWork: Sendable {
  public let kind: UInt32
  public let units: UInt64
}
public struct GraphColumn: Sendable {
  public let name: String
  public let kinds: UInt32
}
public struct GraphMetadata: Sendable {
  public let disposition: GraphDisposition
  public let admittedGeneration: UInt64?
  public let changedGeneration: UInt64?
  public internal(set) var receipts: [GraphReceipt]
  public let reports: [GraphSearchReport]
  public internal(set) var diagnostics: [GraphDiagnostic]
  public let globalWork: [GraphWork]
}
public struct GraphResult: Sendable {
  public let metadata: GraphMetadata
  public let columns: [GraphColumn]
  public let rows: [[GraphValue]]
}
public struct GraphError: Error, Sendable, CustomStringConvertible {
  public var description: String {
    "GraphError(\(reason), disposition: \(String(describing: metadata?.disposition)))"
  }
  public enum Reason: Sendable {
    case native(Int32)
    case invalidResponse(String)
    case invalidRequest(String)
    case closed
  }
  public var code: ZeppelinError? {
    if case .native(let code) = reason { return ZeppelinError(rawValue: code) }
    return nil
  }
  public let reason: Reason
  public let metadata: GraphMetadata?
  init(_ reason: Reason, metadata: GraphMetadata? = nil) {
    self.reason = reason
    self.metadata = metadata
  }
}

public struct GraphSearchReport: Sendable {
  public let call_id: UInt32
  public let kind: UInt32
  public let generation: UInt64
  public let has_requested_tier: UInt32
  public let requested_tier: UInt32
  public let has_actual_tier: UInt32
  public let actual_tier: UInt32
  public let precision: UInt32
  public let coverage: UInt32
  public let vector_leg: UInt32
  public let lexical_leg: UInt32
  public let has_document_epoch: UInt32
  public let has_query_epoch: UInt32
  public let has_tokenizer_epoch: UInt32
  public let cross_score_complete: UInt32
  public let document_epoch: UInt64
  public let query_epoch: UInt64
  public let tokenizer_epoch: UInt64
  public let effective_alpha: Double
  public let normalization_version: UInt32
  public let rules_version: UInt32
  public let candidate_count: UInt64
  public let cross_scored_count: UInt64
  public let fallback_count: UInt64
  public let work: [GraphWork]
}

// Document interpretation uses the existing Swift embedding value contract.
public enum VectorNormalization: Int32, Sendable {
  case none = 0
  case unitL2 = 1
}

public enum EmbeddingRuntime: Int32, Sendable {
  case coreML = 1
  case mlx = 2
  case cpuReference = 3
}

public enum ComputeUnits: Int32, Sendable {
  case cpu = 1
  case cpuAndGPU = 2
  case cpuAndNeuralEngine = 3
  case all = 4
}

public struct EmbeddingTower: Sendable {
  public var modelID: String
  public var modelVersion: String
  public var weightsDigest: Data
  public var dimensions: UInt32
  public var normalization: VectorNormalization
  public var promptPrefix: String
  public var maxTokens: UInt32
  public var runtime: EmbeddingRuntime
  public var computeUnits: ComputeUnits
  public var operatingSystemBuild: String?

  public init(
    modelID: String,
    modelVersion: String,
    weightsDigest: Data,
    dimensions: UInt32,
    normalization: VectorNormalization = .none,
    promptPrefix: String = "",
    maxTokens: UInt32,
    runtime: EmbeddingRuntime,
    computeUnits: ComputeUnits,
    operatingSystemBuild: String? = nil
  ) {
    self.modelID = modelID
    self.modelVersion = modelVersion
    self.weightsDigest = weightsDigest
    self.dimensions = dimensions
    self.normalization = normalization
    self.promptPrefix = promptPrefix
    self.maxTokens = maxTokens
    self.runtime = runtime
    self.computeUnits = computeUnits
    self.operatingSystemBuild = operatingSystemBuild
  }
}
