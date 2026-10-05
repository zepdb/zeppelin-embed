import Foundation
import ZeppelinEmbedGraph

@main
struct InstalledConsumer {
  static func main() async throws {
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
    guard written.metadata.changedGeneration == 1, written.metadata.receipts.count == 3 else {
      throw Failure.invalidResult
    }
    let query = "MATCH (n:Doc)-[:LINK]->(m) WHERE n.title = $title RETURN n.title, $number"
    let result = try await store.cypher(query,
      parameters: ["title": .string("alpha"), "number": .integer(42)])
    guard result.rows == [[.string("alpha"), .integer(42)]],
      result.metadata.admittedGeneration == 1 else { throw Failure.invalidResult }
    try await store.close()
    let reopened = try await ZeppelinGraphStore.open(at: path, mode: .readWrite)
    let durable = try await reopened.cypher(query,
      parameters: ["title": .string("alpha"), "number": .integer(42)])
    guard durable.rows == result.rows, durable.metadata.admittedGeneration == 1 else {
      throw Failure.invalidResult
    }
    try await reopened.close()
    print("installed typed Swift batch/Cypher/reopen PASS")
  }
  enum Failure: Error { case invalidResult }
}
