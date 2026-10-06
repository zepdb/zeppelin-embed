import Foundation

public struct GraphNodeFields: Sendable {
  public var text: Bool
  public var vector: Bool
  public init(text: Bool = false, vector: Bool = false) {
    self.text = text
    self.vector = vector
  }
}
public struct GraphNodesResult: Sendable {
  public let nodes: [GraphNode?]
  public let metadata: GraphMetadata
  init(_ result: GraphResult, count: Int) throws {
    guard result.columns.count == 1, result.rows.count == count else {
      throw GraphError(.invalidResponse("node getter shape"), metadata: result.metadata)
    }
    nodes = try result.rows.map { row in
      guard row.count == 1 else {
        throw GraphError(.invalidResponse("node getter row"), metadata: result.metadata)
      }
      switch row[0] {
      case .null: return nil
      case .node(let node): return node
      default: throw GraphError(.invalidResponse("node getter value"), metadata: result.metadata)
      }
    }
    metadata = result.metadata
  }
}
public struct GraphRelationshipsResult: Sendable {
  public let relationships: [GraphRelationship?]
  public let metadata: GraphMetadata
  init(_ result: GraphResult, count: Int) throws {
    guard result.columns.count == 1, result.rows.count == count else {
      throw GraphError(.invalidResponse("relationship getter shape"), metadata: result.metadata)
    }
    relationships = try result.rows.map { row in
      guard row.count == 1 else {
        throw GraphError(.invalidResponse("relationship getter row"), metadata: result.metadata)
      }
      switch row[0] {
      case .null: return nil
      case .relationship(let relationship): return relationship
      default:
        throw GraphError(.invalidResponse("relationship getter value"), metadata: result.metadata)
      }
    }
    metadata = result.metadata
  }
}
