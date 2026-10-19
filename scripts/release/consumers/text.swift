import Foundation
import ZeppelinEmbed

enum Failure: Error { case check(String) }
func require(_ condition: Bool, _ message: String) throws {
    if !condition { throw Failure.check(message) }
}

let args = CommandLine.arguments
try require(args.count == 3, "expected store path and bundle path")
let store = try await TextStore.open(at: URL(fileURLWithPath: args[1]),
    bundle: URL(fileURLWithPath: args[2]))
let id = DocumentID(high: 0, low: 1)
let mutation = try await store.ingest([TextDocument(id: id, revision: 1, text: "hello world")])
try require(mutation.generation > 0, "ingest generation")
let result = try await store.query("hello world", legs: .dense, k: 1)
try require(result.hits.count == 1, "one dense hit")
guard let hit = result.hits.first else { throw Failure.check("missing hit") }
try require(hit.id == id && hit.text == "hello world", "document identity and text")
try require(hit.score.isFinite && hit.vectorSquaredL2?.isFinite == true, "finite dense score")
try require(hit.lexicalBM25 == nil, "dense leg")
try await store.close()
print("ZE_TEXT_INSTALLED: open, ingest, dense query PASS")
