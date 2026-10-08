#if ZE_GRAPH
import CZeppelinEmbed
import Foundation
@testable import ZeppelinEmbed

enum GraphTestOpenMode { case create, readWrite, readOnly }
func openGraphTestStore(at path: URL, mode: GraphTestOpenMode = .readWrite,
                        documentTower: EmbeddingTower? = nil) async throws -> ZeppelinStore {
    let options = OpenOptions(accessMode: mode == .readOnly ? .readOnly : .readWrite,
                              durabilityMode: .durable, commitTier: .durable, maxResidentBytes: 256 << 20)
    let store: ZeppelinStore
    if let tower = documentTower {
        store = try await ZeppelinStore.openWithEpoch(at: path,
            epoch: Epoch(embedding: EmbeddingEpoch(document: tower, query: tower)), options: options)
    } else {
        store = try await ZeppelinStore.open(at: path, options: options)
    }
    if mode == .create { _ = try await store.enableGraph() }
    return store
}
func makeGraphTestStore(handle: UInt64, calls: GraphNativeCalls) -> ZeppelinStore {
    ZeppelinStore(handle: handle,
        configuration: ReopenConfiguration(path: URL(fileURLWithPath: "/unused"), options: OpenOptions(), epoch: nil),
        graphCalls: calls)
}
// External-C smoke/parity fixtures use the normal Store open contract too.
func openGraphStoreHandle(_ pointer: UnsafePointer<ZeGraphOpenRequest>, _ handle: UnsafeMutablePointer<ze_handle>) -> Int32 {
    let request = pointer.pointee
    if request.mode == 0 { return withUnsafePointer(to: request) { ze_store_create_with_relationship_types($0, nil, 0, handle) } }
    var open = ZeOpenRequest()
    open.abi_size = UInt32(MemoryLayout<ZeOpenRequest>.size)
    open.path = request.path.data; open.path_len = request.path.count
    open.access_mode = request.mode == 2 ? 1 : 0
    open.durability_mode = 1; open.commit_tier = 2
    open.reader_drain_timeout_ms = request.reader_drain_timeout_ms
    open.max_resident_bytes = request.max_resident_bytes; open.max_temp_bytes = UInt64.max
    guard let tower = request.document_tower else { return ze_open(&open, handle) }
    var epoch = ZeEpochRequest()
    epoch.abi_size = UInt32(MemoryLayout<ZeEpochRequest>.size)
    epoch.embedding.document = tower.pointee; epoch.embedding.query = tower.pointee
    return ze_open_with_epoch(&open, &epoch, handle)
}
#endif
