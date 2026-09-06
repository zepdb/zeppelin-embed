import Foundation
import ZeppelinEmbed

@main
struct FiveVectorsSearch {
    static func main() async throws {
        // A store is a persistent directory. This temporary path keeps the
        // example self-cleaning; use an application path to reopen it later.
        let path = FileManager.default.temporaryDirectory
            .appendingPathComponent("zeppelin-swift-example-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: path) }

        // Applications supply document vectors from their chosen embedding
        // model. Four dimensions keep this example readable.
        let vectors: [[Float]] = [
            [0.90, 0.10, 0.05, 0.00],
            [0.85, 0.15, 0.10, 0.05],
            [0.10, 0.90, 0.05, 0.00],
            [0.05, 0.10, 0.90, 0.00],
            [0.00, 0.05, 0.10, 0.90],
        ]

        try await ZeppelinStore.withStore(at: path) { store in
            // Stable IDs identify documents, while revisions order updates.
            let documents = vectors.enumerated().map { index, vector in
                IngestDocument(
                    id: DocumentID(high: 0, low: UInt64(index + 1)),
                    revision: 1,
                    timestamp: Int64(index + 1) * 10,
                    vector: vector
                )
            }
            let mutation = try await store.ingest(documents)
            print("ingested 5 vectors at generation \(mutation.generation)")

            // The query vector must use the same dimension and embedding
            // space. Ask Zeppelin Embed for the nearest three documents.
            let result = try await store.search(
                vector: [0.88, 0.12, 0.07, 0.02],
                options: SearchOptions(k: 3)
            )
            for (index, hit) in result.hits.enumerated() {
                print("\(index + 1). document \(hit.documentID?.low ?? 0), score \(hit.score)")
            }
        }
    }
}
