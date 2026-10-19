#if ZE_TEXT
import CZeppelinEmbed
import Foundation

public enum TextHealthStatus: Int32, Sendable {
    case unchecked = 0, healthy = 1, degraded = 2, unhealthy = 3
}

public struct TextHealth: Sendable {
    public let generation: UInt64
    public let status: TextHealthStatus
    public let pendingDocuments: UInt64
    public let graphCoverage: Double
    public let segmentCount: UInt64
    public let unresolvedFaultCount: UInt64
    public let retainedThrough: Int64?
}

/// Model-backed text API from the single arm64 full XCFramework.
@available(macOS 14.0, *)
public actor TextStore {
    private var handle: UInt64?

    private init(handle: UInt64) { self.handle = handle }
    deinit { if let handle { _ = ze_close(handle) } }

    public static func open(
        at path: URL, bundle: URL, options: OpenOptions = OpenOptions()
    ) async throws -> TextStore {
        let handle = try await ZeppelinStore.runBlocking {
            try ZeppelinStore.prepareDirectory(path, options: options)
            return try Array(path.path.utf8).withUnsafeBufferPointer { pathBytes in
                try Array(bundle.path.utf8).withUnsafeBufferPointer { bundleBytes in
                    var request = ZeTextOpenRequest()
                    request.abi_size = abiSize(ZeTextOpenRequest.self)
                    request.store = openRequest(pathBytes, options)
                    request.bundle_path = bundleBytes.baseAddress
                    request.bundle_path_len = bundleBytes.count
                    var handle: UInt64 = 0
                    try checkZeppelin(ze_text_open(&request, &handle))
                    return handle
                }
            }
        }
        return TextStore(handle: handle)
    }

    /// Resolves a pinned model on a detached task, like ZeppelinStore. Progress
    /// runs synchronously on that task's thread; dispatch UI updates to the main actor.
    public static func open(
        at path: URL, model: TextModel, cacheDirectory: URL? = nil, offline: Bool = false,
        options: OpenOptions = OpenOptions(),
        progress: (@Sendable (TextDownloadProgress) -> Void)? = nil
    ) async throws -> TextStore {
        let handle = try await ZeppelinStore.runBlocking {
            try ZeppelinStore.prepareDirectory(path, options: options)
            let callback = TextProgressCallback(progress)
            return try withExtendedLifetime(callback) {
                try Array(path.path.utf8).withUnsafeBufferPointer { pathBytes in
                    try Array(model.repo.utf8).withUnsafeBufferPointer { repo in
                        try Array(model.revision.utf8).withUnsafeBufferPointer { revision in
                            try Array(model.manifestSHA256.utf8).withUnsafeBufferPointer { pin in
                                try Array((cacheDirectory?.path ?? "").utf8).withUnsafeBufferPointer { cache in
                                    var request = ZeTextOpenModelRequest()
                                    request.abi_size = abiSize(ZeTextOpenModelRequest.self)
                                    request.store = openRequest(pathBytes, options)
                                    request.repo = repo.baseAddress
                                    request.repo_len = repo.count
                                    request.revision = revision.baseAddress
                                    request.revision_len = revision.count
                                    request.manifest_sha256 = pin.baseAddress
                                    request.manifest_sha256_len = pin.count
                                    request.cache_dir = cache.baseAddress
                                    request.cache_dir_len = cache.count
                                    request.offline = offline ? 1 : 0
                                    if progress != nil {
                                        request.user_data = Unmanaged.passUnretained(callback).toOpaque()
                                        request.progress = { context, done, total in
                                            guard let context else { return }
                                            Unmanaged<TextProgressCallback>.fromOpaque(context)
                                                .takeUnretainedValue().callback?(
                                                    TextDownloadProgress(completedBytes: done, totalBytes: total))
                                        }
                                    }
                                    var handle: UInt64 = 0
                                    try checkZeppelin(ze_text_open_model(&request, &handle))
                                    return handle
                                }
                            }
                        }
                    }
                }
            }
        }
        return TextStore(handle: handle)
    }

    /// Explicit, potentially expensive CoreML plan inspection. Never called by query.
    public func observedComputeUnits() async throws -> TextObservedComputeUnits? {
        let handle = try openHandle()
        return try await ZeppelinStore.runBlocking {
            var report = ZeTextObservedComputeUnits()
            report.abi_size = abiSize(ZeTextObservedComputeUnits.self)
            try checkZeppelin(ze_text_observed_compute_units(handle, &report))
            guard report.has_observed_compute_units == 1 else { return nil }
            return TextObservedComputeUnits(cpu: report.cpu_ops, gpu: report.gpu_ops,
                neuralEngine: report.neural_engine_ops, other: report.other_ops)
        }
    }

    public func health() async throws -> TextHealth {
        let handle = try openHandle()
        return try await ZeppelinStore.runBlocking {
            var report = ZeTextHealthReport()
            report.abi_size = abiSize(ZeTextHealthReport.self)
            try checkZeppelin(ze_text_health(handle, &report))
            guard let status = TextHealthStatus(rawValue: report.status) else {
                throw ZeppelinError.internalError
            }
            return TextHealth(generation: report.generation, status: status,
                pendingDocuments: report.pending_docs, graphCoverage: report.graph_coverage,
                segmentCount: report.segment_count, unresolvedFaultCount: report.unresolved_fault_count,
                retainedThrough: report.has_retained_through == 1 ? report.retained_through : nil)
        }
    }

    public func ingest(
        _ documents: [TextDocument], options: TextIngestOptions = TextIngestOptions(),
        deadlineNanoseconds: UInt64 = 0
    ) async throws -> TextIngestReport {
        let handle = try openHandle()
        guard options.embedBatchSize > 0, options.sealEvery > 0, options.channelCapacity > 0 else {
            throw ZeppelinError.invalidArgument
        }
        if case let .tokens(max, overlap) = options.chunkPolicy {
            guard max > 0, overlap >= 0, overlap < max else { throw ZeppelinError.invalidArgument }
        }
        return try await Self.controlled(deadlineNanoseconds: deadlineNanoseconds) { token in
            let bytes = documents.flatMap { Array($0.text.utf8) }
            let offsets = ZeppelinStore.offsets(documents.map { $0.text.utf8.count })
            return try bytes.withUnsafeBufferPointer { bytes in
                let records = documents.enumerated().map { index, document in
                    var record = ZeTextDocument()
                    record.abi_size = abiSize(ZeTextDocument.self)
                    record.doc_id = ZeppelinStore.cDocumentID(document.id)
                    record.revision = document.revision
                    record.text = bytes.baseAddress?.advanced(by: offsets[index])
                    record.text_len = document.text.utf8.count
                    return record
                }
                return try records.withUnsafeBufferPointer { records in
                    var request = ZeTextIngestRequestV2()
                    request.abi_size = abiSize(ZeTextIngestRequestV2.self)
                    request.batch.abi_size = abiSize(ZeTextIngestRequest.self)
                    request.batch.documents = records.baseAddress
                    request.batch.document_count = records.count
                    request.batch.embed_batch_size = options.embedBatchSize
                    request.batch.seal_every = options.sealEvery
                    request.batch.channel_capacity = options.channelCapacity
                    if case let .tokens(max, overlap) = options.chunkPolicy {
                        request.chunk_policy = 1
                        request.chunk_max = max
                        request.chunk_overlap = overlap
                    }
                    request.maintenance_wall_time_ns = options.maintenanceWallTimeNanoseconds
                    request.maintenance_bytes = options.maintenanceBytes
                    request.cancel_token = token
                    request.deadline_ns = deadlineNanoseconds
                    var report = ZeTextIngestReport()
                    report.abi_size = abiSize(ZeTextIngestReport.self)
                    try checkZeppelin(ze_text_ingest_v2(handle, &request, &report))
                    return TextIngestReport(generation: report.generation, documents: report.documents,
                        chunks: report.chunks, tokens: report.tokens, embedBatches: report.embed_batches,
                        seals: report.seals, maxInFlightBatches: report.max_in_flight_batches,
                        allThreadsJoined: report.all_threads_joined == 1, elapsedNanoseconds: report.elapsed_ns)
                }
            }
        }
    }

    /// A nonzero deadline selects the native relative deadline instead of the
    /// Task cancellation token; the ABI does not accept both controls.
    public func query(
        _ text: String, legs: TextLegs = .hybrid, k: Int = 10, tier: SearchTier? = nil,
        deadlineNanoseconds: UInt64 = 0
    ) async throws -> TextQueryResult {
        let handle = try openHandle()
        guard k >= 0 else { throw ZeppelinError.invalidArgument }
        return try await Self.controlled(deadlineNanoseconds: deadlineNanoseconds) { token in
            try Array(text.utf8).withUnsafeBufferPointer { bytes in
                var request = ZeTextQueryRequestV2()
                request.abi_size = abiSize(ZeTextQueryRequestV2.self)
                request.query.abi_size = abiSize(ZeTextQueryRequest.self)
                request.query.text = bytes.baseAddress
                request.query.text_len = bytes.count
                request.query.k = k
                request.query.legs = legs.rawValue
                request.has_tier = tier == nil ? 0 : 1
                request.tier = tier?.rawValue ?? 0
                request.cancel_token = token
                request.deadline_ns = deadlineNanoseconds
                var outcome = ZeTextQueryOutcome()
                outcome.abi_size = abiSize(ZeTextQueryOutcome.self)
                try checkZeppelin(ze_text_query_v2(handle, &request, &outcome))
                // Copy every hit/string before releasing the owned outcome.
                let result: TextQueryResult
                do {
                    guard outcome.result.hit_count == 0 || outcome.result.hits != nil else {
                        throw ZeppelinError.internalError
                    }
                    let hits = try UnsafeBufferPointer(start: outcome.result.hits,
                        count: outcome.result.hit_count).map { hit in
                        guard hit.text_len == 0 || hit.text != nil else { throw ZeppelinError.internalError }
                        return TextHit(id: DocumentID(high: hit.doc_id.high, low: hit.doc_id.low),
                            revision: hit.revision, chunk: hit.chunk,
                            text: String(decoding: UnsafeBufferPointer(start: hit.text, count: hit.text_len), as: UTF8.self),
                            score: hit.score,
                            vectorSquaredL2: hit.has_vector_score == 1 ? hit.vector_squared_l2 : nil,
                            lexicalBM25: hit.has_lexical_score == 1 ? hit.lexical_bm25 : nil)
                    }
                    result = TextQueryResult(hits: hits, embeddingEpoch: outcome.result.embedding_epoch,
                        tokenizerEpoch: outcome.result.tokenizer_epoch)
                } catch {
                    try checkZeppelin(ze_text_query_outcome_free(&outcome))
                    throw error
                }
                try checkZeppelin(ze_text_query_outcome_free(&outcome))
                return result
            }
        }
    }

    public func delete(_ ids: [DocumentID]) async throws -> MutationReport {
        let handle = try openHandle()
        return try await ZeppelinStore.runBlocking {
            try ids.map(ZeppelinStore.cDocumentID).withUnsafeBufferPointer { ids in
                var request = ZeDeleteRequest()
                request.abi_size = abiSize(ZeDeleteRequest.self)
                request.doc_ids = ids.baseAddress
                request.doc_id_count = ids.count
                var report = ZeMutationReport()
                report.abi_size = abiSize(ZeMutationReport.self)
                try checkZeppelin(ze_text_delete(handle, &request, &report))
                return MutationReport(sequence: report.sequence, generation: report.generation)
            }
        }
    }

    /// Runs one bounded maintenance slice. The host decides when to continue.
    public func maintain(wallTimeNanoseconds: UInt64, byteBudget: UInt64) async throws -> MaintenanceReport {
        let handle = try openHandle()
        return try await ZeppelinStore.runBlocking {
            var request = ZeMaintainRequest()
            request.abi_size = abiSize(ZeMaintainRequest.self)
            request.wall_time_ns = wallTimeNanoseconds
            request.bytes = byteBudget
            var report = ZeMaintainReport()
            report.abi_size = abiSize(ZeMaintainReport.self)
            try checkZeppelin(ze_text_maintain_budgeted(handle, &request, &report))
            guard let status = MaintenanceStatus(rawValue: report.status) else { throw ZeppelinError.internalError }
            return MaintenanceReport(graphsBuilt: report.graphs_built, bytesConsumed: report.bytes_consumed,
                checkpointsResumed: report.checkpoints_resumed, status: status)
        }
    }

    private nonisolated static func controlled<Result: Sendable>(
        deadlineNanoseconds: UInt64,
        _ body: @escaping @Sendable (UInt64) throws -> Result
    ) async throws -> Result {
        if Task.isCancelled { throw ZeppelinError.cancelled }
        // Native controls are mutually exclusive. Deadline mode retains its
        // native timeout error; ordinary calls bridge Task cancellation.
        if deadlineNanoseconds != 0 {
            return try await ZeppelinStore.runBlocking { try body(0) }
        }
        let token = try await ZeppelinCancellationToken.create()
        let raw = try token.rawValue()
        return try await withTaskCancellationHandler {
            try await ZeppelinStore.runBlocking {
                try withExtendedLifetime(token) { try body(raw) }
            }
        } onCancel: {
            _ = ze_cancel_token_cancel(raw)
        }
    }

    public func close() async throws {
        guard let handle else { return }
        self.handle = nil
        try await ZeppelinStore.runBlocking { try checkZeppelin(ze_close(handle)) }
    }

    private func openHandle() throws -> UInt64 {
        guard let handle else { throw ZeppelinError.closed }
        return handle
    }

    private nonisolated static func openRequest(
        _ path: UnsafeBufferPointer<UInt8>, _ options: OpenOptions
    ) -> ZeOpenRequest {
        var request = ZeOpenRequest()
        request.abi_size = abiSize(ZeOpenRequest.self)
        request.path = path.baseAddress
        request.path_len = path.count
        request.access_mode = options.accessMode.rawValue
        request.durability_mode = options.durabilityMode.rawValue
        request.commit_tier = options.commitTier.rawValue
        request.reader_drain_timeout_ms = options.readerDrainTimeoutMilliseconds
        request.max_resident_bytes = options.maxResidentBytes
        request.max_temp_bytes = options.maxTemporaryBytes
        return request
    }
}
private final class TextProgressCallback: Sendable {
    let callback: (@Sendable (TextDownloadProgress) -> Void)?
    init(_ callback: (@Sendable (TextDownloadProgress) -> Void)?) { self.callback = callback }
}
#endif
