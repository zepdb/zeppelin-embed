#if ZE_GRAPH
import XCTest
@testable import ZeppelinEmbed

final class StoreGraphTests: XCTestCase {
    func testOneHandleSharesDocumentsAndGraph() async throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let generation = try await store.enableGraph()
        let repeated = try await store.enableGraph()
        XCTAssertEqual(repeated, generation)
        var batch = GraphBatch()
        let id = DocumentID(high: 7, low: 9)
        batch.node(key: GraphKey(namespace: "", key: "doc"), revision: 1,
                   .create(GraphNodeImage(text: "hello", id: id, timestamp: 42, metadata: Data([1, 2]))))
        _ = try await store.graphApply(batch)
        let document = try await store.get([id]).documents.first ?? nil
        XCTAssertEqual(document?.timestamp, 42)
        XCTAssertEqual(document?.metadata, Data([1, 2]))
        let nodes = try await store.getNodes([id], fields: GraphNodeFields(text: true))
        XCTAssertEqual(nodes.nodes.first??.id, id)
        XCTAssertEqual(nodes.nodes.first??.text, "hello")
        try await store.close()
    }
    func testDocumentFieldsAndVectorUseStoreEpoch() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let tower = EmbeddingTower(modelID: "fixture", modelVersion: "1", weightsDigest: Data([1]),
            dimensions: 2, maxTokens: 10, runtime: .cpuReference, computeUnits: .cpu)
        let store = try await ZeppelinStore.openNamespace(root: root, name: "graph", spec: NamespaceSpec(
            attributes: [AttributeDefinition(id: 1, name: "label", type: .rawString, nullable: true)],
            vectorSpace: VectorSpace(dimensions: 2, normalization: .none),
            epoch: Epoch(embedding: EmbeddingEpoch(document: tower, query: tower))),
            options: OpenOptions(maxResidentBytes: 256 << 20))
        _ = try await store.enableGraph()
        let id = DocumentID(high: UInt64.max, low: 13)
        var batch = GraphBatch()
        let a = batch.node(key: GraphKey(namespace: "", key: "a"), revision: 1,
            .create(GraphNodeImage(text: "α", vector: [1, 0], id: id, timestamp: -42,
                                  attributes: [1: .string("日本語")], metadata: Data([0, 255]))))
        let b = batch.node(key: GraphKey(namespace: "", key: "b"), revision: 1, .create(GraphNodeImage()))
        batch.relationship(key: GraphKey(namespace: "", key: "edge"), revision: 1,
            .create(GraphRelationshipImage(type: "LINK"), .local(a), .local(b)))
        let result = try await store.graphApply(batch)
        let fetched = try await store.get([id])
        XCTAssertEqual(fetched.generation, result.metadata.changedGeneration)
        let doc = try XCTUnwrap(fetched.documents[0])
        XCTAssertEqual(doc.vector, [1, 0])
        XCTAssertEqual(doc.timestamp, -42)
        XCTAssertEqual(doc.attributes, [1: .string("日本語")])
        XCTAssertEqual(doc.metadata, Data([0, 255]))
        var invalid = GraphBatch()
        invalid.node(key: GraphKey(namespace: "", key: "bad"), revision: 1,
            .create(GraphNodeImage(vector: [1], id: DocumentID(high: 0, low: 99))))
        do { _ = try await store.graphApply(invalid); XCTFail("wrong dimension") }
        catch let error as GraphError { XCTAssertEqual(error.metadata?.disposition, .notCommitted) }
        var put = GraphBatch()
        put.node(key: GraphKey(namespace: "", key: "a"), revision: 2,
            .put(id, GraphNodeImage(text: "replacement", vector: [0, 1], timestamp: 0,
                                   attributes: [:], metadata: Data())))
        _ = try await store.graphApply(put)
        let replaced = try await store.get([id])
        XCTAssertEqual(replaced.documents[0]?.timestamp, 0)
        XCTAssertEqual(replaced.documents[0]?.attributes, [:])
        XCTAssertEqual(replaced.documents[0]?.metadata, Data())
        try await store.close()
    }

    func testQuiesceResumePreservesGraph() async throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path, options: OpenOptions(maxResidentBytes: 256 << 20))
        _ = try await store.enableGraph()
        _ = try await store.cypher("CREATE (:Doc {title: 'durable'})")
        let before = try await store.cypher("MATCH (n:Doc) RETURN n.title")
        try await store.quiesce()
        try await store.resume()
        let after = try await store.cypher("MATCH (n:Doc) RETURN n.title")
        XCTAssertEqual(after.rows, before.rows)
        XCTAssertEqual(after.metadata.admittedGeneration, before.metadata.admittedGeneration)
        try await store.close()
    }

}

#endif
