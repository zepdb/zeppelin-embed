import CZeppelinEmbed
import Foundation

enum Failure: Error { case check(String) }
func check(_ code: Int32) throws {
    if code != ZE_OK.rawValue { throw Failure.check(String(cString: ze_error_code_name(code))) }
}
func require(_ condition: Bool, _ message: String) throws {
    if !condition { throw Failure.check(message) }
}

let args = CommandLine.arguments
try require(args.count == 3, "expected store path and bundle path")
var handle: ze_handle = 0
try Array(args[1].utf8).withUnsafeBufferPointer { path in
    try Array(args[2].utf8).withUnsafeBufferPointer { bundle in
        var open = ZeTextOpenRequest()
        open.abi_size = UInt32(MemoryLayout<ZeTextOpenRequest>.size)
        open.store.abi_size = UInt32(MemoryLayout<ZeOpenRequest>.size)
        open.store.path = path.baseAddress
        open.store.path_len = path.count
        open.store.commit_tier = 1
        open.store.reader_drain_timeout_ms = 100
        open.store.max_resident_bytes = UInt64.max
        open.store.max_temp_bytes = UInt64.max
        open.bundle_path = bundle.baseAddress
        open.bundle_path_len = bundle.count
        try check(ze_text_open(&open, &handle))
    }
}
try Array("hello world".utf8).withUnsafeBufferPointer { text in
    var document = ZeTextDocument()
    document.abi_size = UInt32(MemoryLayout<ZeTextDocument>.size)
    document.doc_id.low = 1
    document.revision = 1
    document.text = text.baseAddress
    document.text_len = text.count
    var mutation = ZeMutationReport()
    mutation.abi_size = UInt32(MemoryLayout<ZeMutationReport>.size)
    try withUnsafePointer(to: &document) { pointer in
        var ingest = ZeTextIngestRequest()
        ingest.abi_size = UInt32(MemoryLayout<ZeTextIngestRequest>.size)
        ingest.documents = pointer
        ingest.document_count = 1
        ingest.embed_batch_size = 1
        ingest.seal_every = 1
        ingest.channel_capacity = 1
        try check(ze_text_ingest(handle, &ingest, &mutation))
    }
    try require(mutation.generation > 0, "ingest generation")
    var query = ZeTextQueryRequest()
    query.abi_size = UInt32(MemoryLayout<ZeTextQueryRequest>.size)
    query.text = text.baseAddress
    query.text_len = text.count
    query.k = 1
    query.legs = 0 // Dense only.
    var result = ZeTextQueryResult()
    result.abi_size = UInt32(MemoryLayout<ZeTextQueryResult>.size)
    try check(ze_text_query(handle, &query, &result))
    try require(result.hit_count == 1, "one dense hit")
    guard let hits = result.hits else { throw Failure.check("missing hit buffer") }
    let hit = hits.pointee
    try require(hit.doc_id.high == 0 && hit.doc_id.low == 1, "document identity")
    try require(hit.score.isFinite && hit.vector_squared_l2.isFinite, "finite dense score")
    try require(hit.has_vector_score == 1 && hit.has_lexical_score == 0, "dense leg")
    try check(ze_text_query_result_free(&result))
}
try check(ze_close(handle))
print("ZE_TEXT_INSTALLED: open, ingest, dense query PASS")
