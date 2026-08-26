import CZeppelinEmbed
import Foundation

private struct ReopenConfiguration: Sendable {
    var path: URL
    var options: OpenOptions
    var epoch: Epoch?
}

@available(macOS 14.0, iOS 17.0, *)
public actor ZeppelinStore {
    private var handle: UInt64?
    private let configuration: ReopenConfiguration

    private init(handle: UInt64, configuration: ReopenConfiguration) {
        self.handle = handle
        self.configuration = configuration
    }

    deinit {
        if let handle {
            _ = ze_close(handle)
        }
    }

    public static func open(
        at path: URL,
        options: OpenOptions = OpenOptions()
    ) async throws -> ZeppelinStore {
        try await open(configuration: ReopenConfiguration(path: path, options: options, epoch: nil))
    }

    public static func openWithEpoch(
        at path: URL,
        epoch: Epoch,
        options: OpenOptions = OpenOptions()
    ) async throws -> ZeppelinStore {
        try await open(
            configuration: ReopenConfiguration(path: path, options: options, epoch: epoch)
        )
    }

    public static func withStore<Result: Sendable>(
        at path: URL,
        options: OpenOptions = OpenOptions(),
        _ operation: @Sendable (ZeppelinStore) async throws -> Result
    ) async throws -> Result {
        let store = try await open(at: path, options: options)
        do {
            let result = try await operation(store)
            try await store.close()
            return result
        } catch {
            try? await store.close()
            throw error
        }
    }

    public func close() async throws {
        guard let current = handle else {
            return
        }
        handle = nil
        try await Self.runBlocking {
            try checkZeppelin(ze_close(current))
        }
    }

    /// Releases the ABI handle so the host can make the store directory
    /// unavailable. ABI v1 has no lightweight quiesce operation, so this is
    /// deliberately the heavier close half of a close-and-reopen pair.
    public func quiesce() async throws {
        try await close()
    }

    /// Reopens the exact path, options, and optional epoch captured at open.
    public func resume() async throws {
        guard handle == nil else {
            return
        }
        handle = try await Self.openRaw(configuration: configuration)
    }

    public func state() async throws -> StoreState {
        let current = try openHandle()
        return try await Self.runBlocking {
            var report = ZeStateReport()
            report.abi_size = abiSize(ZeStateReport.self)
            try checkZeppelin(ze_state(current, &report))
            guard let state = StoreState(rawValue: report.state) else {
                throw ZeppelinError.internalError
            }
            return state
        }
    }

    public func stats() async throws -> StoreStats {
        let current = try openHandle()
        return try await Self.runBlocking {
            var report = ZeStatsReport()
            report.abi_size = abiSize(ZeStatsReport.self)
            try checkZeppelin(ze_stats(current, &report))
            return StoreStats(
                residentOwnedBytes: report.resident_owned_bytes,
                mappedBytes: report.mapped_bytes,
                mappedResidentBytes: report.mapped_resident_bytes,
                segmentBytes: report.segment_bytes,
                activeSegmentBytes: report.active_segment_bytes,
                activeRowCount: report.active_row_count,
                tombstoneCount: report.tombstone_count,
                tombstoneBytes: report.tombstone_bytes,
                walBytes: report.wal_bytes,
                cacheBytes: report.cache_bytes,
                temporaryBytes: report.temporary_bytes,
                queryPoolBytes: report.query_pool_bytes,
                openFiles: report.open_files,
                activeQueries: report.active_queries,
                activeSnapshotLeases: report.active_snapshot_leases,
                physicalFootprint: report.has_phys_footprint == 1 ? report.phys_footprint : nil
            )
        }
    }

    public func ingest(_ documents: [IngestDocument]) async throws -> MutationReport {
        let current = try openHandle()
        let vectors = documents.flatMap(\.vector)
        let metadata = documents.flatMap { Array($0.metadata) }
        let texts = documents.flatMap { document in
            document.text.map { Array($0.utf8) } ?? []
        }
        let vectorOffsets = Self.offsets(documents.map { $0.vector.count })
        let metadataOffsets = Self.offsets(documents.map { $0.metadata.count })
        let textLengths = documents.map { $0.text.map { $0.utf8.count } ?? 0 }
        let textOffsets = Self.offsets(textLengths)

        return try vectors.withUnsafeBufferPointer { vectorBuffer in
            try metadata.withUnsafeBufferPointer { metadataBuffer in
                try texts.withUnsafeBufferPointer { textBuffer in
                    let records = documents.enumerated().map { index, document in
                        var record = ZeIngestDocument()
                        record.abi_size = abiSize(ZeIngestDocument.self)
                        record.doc_id = Self.cDocumentID(document.id)
                        record.revision = document.revision
                        record.timestamp = document.timestamp
                        record.vector = Self.pointer(
                            vectorBuffer.baseAddress,
                            offset: vectorOffsets[index],
                            count: document.vector.count
                        )
                        record.vector_len = document.vector.count
                        record.metadata = Self.pointer(
                            metadataBuffer.baseAddress,
                            offset: metadataOffsets[index],
                            count: document.metadata.count
                        )
                        record.metadata_len = document.metadata.count
                        record.text = Self.pointer(
                            textBuffer.baseAddress,
                            offset: textOffsets[index],
                            count: textLengths[index]
                        )
                        record.text_len = textLengths[index]
                        return record
                    }
                    return try records.withUnsafeBufferPointer { recordBuffer in
                        var request = ZeIngestRequest()
                        request.abi_size = abiSize(ZeIngestRequest.self)
                        request.documents = recordBuffer.baseAddress
                        request.document_count = records.count
                        request.dimension = documents.first?.vector.count ?? 0
                        var report = ZeMutationReport()
                        report.abi_size = abiSize(ZeMutationReport.self)
                        try checkZeppelin(ze_ingest(current, &request, &report))
                        return MutationReport(
                            sequence: report.sequence,
                            generation: report.generation
                        )
                    }
                }
            }
        }
    }

    public func delete(_ documentIDs: [DocumentID]) async throws -> MutationReport {
        let current = try openHandle()
        return try await Self.runBlocking {
            let ids = documentIDs.map(Self.cDocumentID)
            return try ids.withUnsafeBufferPointer { buffer in
                var request = ZeDeleteRequest()
                request.abi_size = abiSize(ZeDeleteRequest.self)
                request.doc_ids = buffer.baseAddress
                request.doc_id_count = ids.count
                var report = ZeMutationReport()
                report.abi_size = abiSize(ZeMutationReport.self)
                try checkZeppelin(ze_delete(current, &request, &report))
                return MutationReport(sequence: report.sequence, generation: report.generation)
            }
        }
    }

    public func search(
        vector: [Float],
        options: SearchOptions
    ) async throws -> SearchResult {
        let current = try openHandle()
        let cancellationToken = try options.cancellationToken?.rawValue() ?? 0
        return try await Self.runBlocking {
            try vector.withUnsafeBufferPointer { vectorBuffer in
                var request = ZeSearchRequest()
                request.abi_size = abiSize(ZeSearchRequest.self)
                request.vector = vectorBuffer.baseAddress
                request.vector_len = vector.count
                request.dimension = vector.count
                request.k = options.k
                request.thread_budget = options.threadBudget
                request.has_tier = options.tier == nil ? 0 : 1
                request.tier = options.tier?.rawValue ?? 0
                request.graph_profile = options.graphProfile.rawValue
                request.graph_ef = options.graphEF
                request.graph_seed = options.graphSeed
                request.cancel_token = cancellationToken
                request.deadline_ns = options.deadlineNanoseconds

                var result = ZeSearchResult()
                result.abi_size = abiSize(ZeSearchResult.self)
                defer { _ = ze_search_result_free(&result) }
                try checkZeppelin(ze_search(current, &request, &result))
                let hits = try Self.copySearchHits(result)
                return SearchResult(
                    hits: hits,
                    generation: result.generation,
                    diagnostics: SearchDiagnostics(
                        dimensionsTouched: result.dims_touched,
                        bytesRead: result.bytes_read,
                        threadsUsed: result.threads_used,
                        graphSegmentsTraversed: result.graph_segments_traversed,
                        graphValidations: result.graph_validations,
                        graphEntrySeedDiscoveries: result.graph_entry_seed_discoveries,
                        graphVisitedEpochClears: result.graph_visited_epoch_clears,
                        graphCandidatesScored: result.graph_candidates_scored,
                        graphCandidatesRescored: result.graph_candidates_rescored,
                        graphSegmentsPrunedByBound: result.graph_segments_pruned_by_bound
                    )
                )
            }
        }
    }

    @_spi(Testing)
    public func triggerABIPanicProbeForTesting() async throws {
        let current = try openHandle()
        try await Self.runBlocking {
            let vector = [1.0 as Float]
            try vector.withUnsafeBufferPointer { buffer in
                var request = ZeSearchRequest()
                request.abi_size = abiSize(ZeSearchRequest.self)
                request.abi_reserved = 0x5041_4e49
                request.vector = buffer.baseAddress
                request.vector_len = 1
                request.dimension = 1
                request.k = 1
                var result = ZeSearchResult()
                result.abi_size = abiSize(ZeSearchResult.self)
                defer { _ = ze_search_result_free(&result) }
                try checkZeppelin(ze_search(current, &request, &result))
            }
        }
    }

    public func query(
        vector: [Float]? = nil,
        text: String? = nil,
        options: QueryOptions
    ) async throws -> QueryResult {
        try await executeQuery(vector: vector, text: text, options: options, onFFIEntry: nil)
    }

    @_spi(Testing)
    public func queryForCancellationTesting(
        vector: [Float]? = nil,
        text: String? = nil,
        options: QueryOptions,
        onFFIEntry: @escaping @Sendable () -> Void
    ) async throws -> QueryResult {
        try await executeQuery(
            vector: vector,
            text: text,
            options: options,
            onFFIEntry: onFFIEntry
        )
    }

    private func executeQuery(
        vector: [Float]?,
        text: String?,
        options: QueryOptions,
        onFFIEntry: (@Sendable () -> Void)?
    ) async throws -> QueryResult {
        let current = try openHandle()
        let cancellationToken = try options.cancellationToken?.rawValue() ?? 0
        return try await Self.runBlocking {
            let vector = vector ?? []
            let textBytes = text.map { Array($0.utf8) } ?? []
            return try vector.withUnsafeBufferPointer { vectorBuffer in
                try textBytes.withUnsafeBufferPointer { textBuffer in
                    var request = ZeQueryRequest()
                    request.abi_size = abiSize(ZeQueryRequest.self)
                    request.vector = vectorBuffer.baseAddress
                    request.vector_len = vector.count
                    request.dimension = vector.count
                    request.text = textBuffer.baseAddress
                    request.text_len = textBytes.count
                    request.k = options.k
                    request.thread_budget = options.threadBudget
                    request.has_tier = options.tier == nil ? 0 : 1
                    request.tier = options.tier?.rawValue ?? 0
                    request.graph_profile = options.graphProfile.rawValue
                    request.graph_ef = options.graphEF
                    request.graph_seed = options.graphSeed
                    request.has_alpha = options.alpha == nil ? 0 : 1
                    request.rules_enabled = options.rulesEnabled ? 1 : 0
                    request.alpha = options.alpha ?? 0
                    request.has_max_rounds = options.maximumRounds == nil ? 0 : 1
                    request.quoted_phrase = options.quotedPhrase ? 1 : 0
                    request.max_rounds = options.maximumRounds ?? 0
                    request.identifier_token = options.identifierToken ? 1 : 0
                    request.has_rarest_exact_document_frequency =
                        options.rarestExactDocumentFrequency == nil ? 0 : 1
                    request.rarest_exact_document_frequency =
                        options.rarestExactDocumentFrequency ?? 0
                    request.cancel_token = cancellationToken
                    request.deadline_ns = options.deadlineNanoseconds

                    var result = ZeQueryResult()
                    result.abi_size = abiSize(ZeQueryResult.self)
                    defer { _ = ze_query_result_free(&result) }
                    onFFIEntry?()
                    try checkZeppelin(ze_query(current, &request, &result))
                    guard let mode = QueryMode(rawValue: result.mode) else {
                        throw ZeppelinError.internalError
                    }
                    let fusion = result.has_fusion == 1
                        ? FusionMethod(rawValue: result.fusion_method)
                        : nil
                    if result.has_fusion == 1 && fusion == nil {
                        throw ZeppelinError.internalError
                    }
                    return QueryResult(
                        hits: try Self.copyQueryHits(result),
                        generation: result.generation,
                        mode: mode,
                        diagnostics: QueryDiagnostics(
                            approximate: result.approximate == 1,
                            exactRescore: result.exact_rescore == 1,
                            budgetExhausted: result.budget_exhausted == 1,
                            fusionMethod: fusion,
                            effectiveAlpha: fusion == nil ? nil : result.effective_alpha,
                            fusionRounds: fusion == nil ? nil : result.fusion_rounds,
                            embeddingEpoch: result.has_embedding_epoch == 1
                                ? result.embedding_epoch : nil,
                            tokenizerEpoch: result.has_tokenizer_epoch == 1
                                ? result.tokenizer_epoch : nil,
                            dimensionsTouched: result.dims_touched,
                            bytesRead: result.bytes_read,
                            documentsEvaluated: result.docs_evaluated,
                            postingsDecoded: result.postings_decoded
                        )
                    )
                }
            }
        }
    }

    public func seal(cancellationToken: ZeppelinCancellationToken? = nil) async throws
        -> GenerationReport
    {
        let current = try openHandle()
        let token = try cancellationToken?.rawValue() ?? 0
        return try await Self.runBlocking {
            var request = ZeSealRequest()
            request.abi_size = abiSize(ZeSealRequest.self)
            request.cancel_token = token
            var report = ZeGenerationReport()
            report.abi_size = abiSize(ZeGenerationReport.self)
            try checkZeppelin(ze_seal(current, &request, &report))
            return GenerationReport(generation: report.generation)
        }
    }

    public func dropPartition(from start: Int64, to end: Int64) async throws
        -> PartitionReport
    {
        let current = try openHandle()
        return try await Self.runBlocking {
            var request = ZeDropPartitionRequest()
            request.abi_size = abiSize(ZeDropPartitionRequest.self)
            request.start_ts = start
            request.end_ts = end
            var report = ZePartitionReport()
            report.abi_size = abiSize(ZePartitionReport.self)
            try checkZeppelin(ze_drop_partition(current, &request, &report))
            return Self.partitionReport(report)
        }
    }

    public func applyRetention(window: Int64, now: Int64) async throws -> PartitionReport {
        let current = try openHandle()
        return try await Self.runBlocking {
            var request = ZeRetentionRequest()
            request.abi_size = abiSize(ZeRetentionRequest.self)
            request.window = window
            request.now_ts = now
            var report = ZePartitionReport()
            report.abi_size = abiSize(ZePartitionReport.self)
            try checkZeppelin(ze_apply_retention(current, &request, &report))
            return Self.partitionReport(report)
        }
    }

    public func purge(_ documentIDs: [DocumentID]) async throws -> PurgeToken {
        let current = try openHandle()
        return try await Self.runBlocking {
            let ids = documentIDs.map(Self.cDocumentID)
            return try ids.withUnsafeBufferPointer { buffer in
                var request = ZePurgeRequest()
                request.abi_size = abiSize(ZePurgeRequest.self)
                request.doc_ids = buffer.baseAddress
                request.doc_id_count = ids.count
                var report = ZePurgeTokenReport()
                report.abi_size = abiSize(ZePurgeTokenReport.self)
                try checkZeppelin(ze_purge(current, &request, &report))
                return PurgeToken(
                    tokenID: report.token_id,
                    generation: report.generation,
                    unknownIDCount: report.unknown_id_count,
                    isNoOp: report.is_no_op == 1
                )
            }
        }
    }

    public func awaitPhysicalPurge(_ token: PurgeToken) async throws -> PurgeReport {
        let current = try openHandle()
        return try await Self.runBlocking {
            var request = ZeAwaitPurgeRequest()
            request.abi_size = abiSize(ZeAwaitPurgeRequest.self)
            request.token_id = token.tokenID
            var report = ZePurgeReport()
            report.abi_size = abiSize(ZePurgeReport.self)
            try checkZeppelin(ze_await_physical_purge(current, &request, &report))
            return PurgeReport(
                generation: report.generation,
                segmentsRewritten: report.segments_rewritten,
                unknownIDCount: report.unknown_id_count,
                walRewritten: report.wal_rewritten == 1,
                isNoOp: report.is_no_op == 1
            )
        }
    }

    public func maintain(wallTimeNanoseconds: UInt64, bytes: UInt64) async throws
        -> MaintenanceReport
    {
        let current = try openHandle()
        return try await Self.runBlocking {
            var request = ZeMaintainRequest()
            request.abi_size = abiSize(ZeMaintainRequest.self)
            request.wall_time_ns = wallTimeNanoseconds
            request.bytes = bytes
            var report = ZeMaintainReport()
            report.abi_size = abiSize(ZeMaintainReport.self)
            try checkZeppelin(ze_maintain(current, &request, &report))
            guard let status = MaintenanceStatus(rawValue: report.status) else {
                throw ZeppelinError.internalError
            }
            return MaintenanceReport(
                graphsBuilt: report.graphs_built,
                bytesConsumed: report.bytes_consumed,
                checkpointsResumed: report.checkpoints_resumed,
                status: status
            )
        }
    }

    public static func epochIdentity(_ epoch: Epoch) async throws -> EpochIdentity {
        try await runBlocking {
            try Self.withEpochRequest(epoch) { request in
                var report = ZeEpochIdentity()
                report.abi_size = abiSize(ZeEpochIdentity.self)
                try checkZeppelin(ze_epoch_identity(request, &report))
                return EpochIdentity(
                    embeddingEpoch: report.embedding_epoch,
                    tokenizerEpoch: report.tokenizer_epoch
                )
            }
        }
    }

    public func currentEpoch() async throws -> EpochIdentity {
        let current = try openHandle()
        return try await Self.runBlocking {
            var report = ZeEpochIdentity()
            report.abi_size = abiSize(ZeEpochIdentity.self)
            try checkZeppelin(ze_epoch_current(current, &report))
            return EpochIdentity(
                embeddingEpoch: report.embedding_epoch,
                tokenizerEpoch: report.tokenizer_epoch
            )
        }
    }

    public func switchAlias(to epoch: Epoch) async throws -> EpochAliasReport {
        let current = try openHandle()
        return try await Self.runBlocking {
            try Self.withEpochRequest(epoch) { request in
                var report = ZeEpochAliasReport()
                report.abi_size = abiSize(ZeEpochAliasReport.self)
                try checkZeppelin(ze_epoch_switch_alias(current, request, &report))
                return EpochAliasReport(
                    generation: report.generation,
                    previous: EpochIdentity(
                        embeddingEpoch: report.previous_embedding_epoch,
                        tokenizerEpoch: report.previous_tokenizer_epoch
                    ),
                    published: EpochIdentity(
                        embeddingEpoch: report.published_embedding_epoch,
                        tokenizerEpoch: report.published_tokenizer_epoch
                    ),
                    manifestCommitted: report.manifest_committed == 1
                )
            }
        }
    }

    public func dropEpoch(_ epoch: Epoch) async throws -> EpochDropReport {
        let current = try openHandle()
        return try await Self.runBlocking {
            try Self.withEpochRequest(epoch) { request in
                var report = ZeEpochDropReport()
                report.abi_size = abiSize(ZeEpochDropReport.self)
                try checkZeppelin(ze_epoch_drop(current, request, &report))
                return EpochDropReport(
                    generation: report.generation,
                    segmentsDropped: report.segments_dropped,
                    bytesReclaimed: report.bytes_reclaimed
                )
            }
        }
    }

    private static func open(configuration: ReopenConfiguration) async throws -> ZeppelinStore {
        let handle = try await openRaw(configuration: configuration)
        return ZeppelinStore(handle: handle, configuration: configuration)
    }

    private static func openRaw(configuration: ReopenConfiguration) async throws -> UInt64 {
        try await runBlocking {
            try prepareDirectory(configuration.path, options: configuration.options)
            let path = Array(configuration.path.path.utf8)
            return try path.withUnsafeBufferPointer { pathBuffer in
                var request = ZeOpenRequest()
                request.abi_size = abiSize(ZeOpenRequest.self)
                request.path = pathBuffer.baseAddress
                request.path_len = path.count
                request.access_mode = configuration.options.accessMode.rawValue
                request.durability_mode = configuration.options.durabilityMode.rawValue
                request.commit_tier = configuration.options.commitTier.rawValue
                request.reader_drain_timeout_ms =
                    configuration.options.readerDrainTimeoutMilliseconds
                request.max_resident_bytes = configuration.options.maxResidentBytes
                request.max_temp_bytes = configuration.options.maxTemporaryBytes
                var handle: UInt64 = 0
                if let epoch = configuration.epoch {
                    try withEpochRequest(epoch) { epochRequest in
                        try checkZeppelin(ze_open_with_epoch(&request, epochRequest, &handle))
                    }
                } else {
                    try checkZeppelin(ze_open(&request, &handle))
                }
                return handle
            }
        }
    }

    private func openHandle() throws -> UInt64 {
        guard let handle else {
            throw ZeppelinError.closed
        }
        return handle
    }

    private nonisolated static func runBlocking<Result: Sendable>(
        _ operation: @escaping @Sendable () throws -> Result
    ) async throws -> Result {
        try await Task.detached(operation: operation).value
    }

    private nonisolated static func prepareDirectory(_ path: URL, options: OpenOptions) throws {
        do {
            if options.accessMode == .readWrite {
                try FileManager.default.createDirectory(
                    at: path,
                    withIntermediateDirectories: true
                )
            }
            if options.excludeFromBackup {
                var mutablePath = path
                var values = URLResourceValues()
                values.isExcludedFromBackup = true
                try mutablePath.setResourceValues(values)
            }
        } catch {
            throw ZeppelinError.io
        }
    }

    private nonisolated static func cDocumentID(_ id: DocumentID) -> ZeDocId {
        var raw = ZeDocId()
        raw.high = id.high
        raw.low = id.low
        return raw
    }

    private nonisolated static func offsets(_ lengths: [Int]) -> [Int] {
        var next = 0
        return lengths.map { length in
            defer { next += length }
            return next
        }
    }

    private nonisolated static func pointer<Element>(
        _ base: UnsafePointer<Element>?,
        offset: Int,
        count: Int
    ) -> UnsafePointer<Element>? {
        guard count > 0, let base else {
            return nil
        }
        return base.advanced(by: offset)
    }

    private nonisolated static func partitionReport(_ report: ZePartitionReport)
        -> PartitionReport
    {
        PartitionReport(
            generation: report.generation,
            segmentsDropped: report.segments_dropped,
            bytesReclaimed: report.bytes_reclaimed,
            straddlersSkipped: report.straddlers_skipped,
            isNoOp: report.is_no_op == 1
        )
    }

    private nonisolated static func copySearchHits(_ result: ZeSearchResult) throws
        -> [SearchHit]
    {
        if result.hit_count == 0 {
            return []
        }
        guard let base = result.hits else {
            throw ZeppelinError.internalError
        }
        return UnsafeBufferPointer(start: base, count: result.hit_count).map { hit in
            var segment = hit.segment_id
            let segmentID = withUnsafeBytes(of: &segment) { Array($0.prefix(16)) }
            return SearchHit(
                sourceKind: hit.source_kind,
                segmentID: segmentID,
                localRow: hit.local_row,
                documentID: hit.has_document == 1
                    ? DocumentID(high: hit.doc_id.high, low: hit.doc_id.low) : nil,
                revision: hit.has_document == 1 ? hit.revision : nil,
                score: hit.score
            )
        }
    }

    private nonisolated static func copyQueryHits(_ result: ZeQueryResult) throws -> [QueryHit] {
        if result.hit_count == 0 {
            return []
        }
        guard let base = result.hits else {
            throw ZeppelinError.internalError
        }
        return UnsafeBufferPointer(start: base, count: result.hit_count).map { hit in
            QueryHit(
                documentID: hit.has_document == 1
                    ? DocumentID(high: hit.doc_id.high, low: hit.doc_id.low) : nil,
                revision: hit.has_revision == 1 ? hit.revision : nil,
                score: hit.score,
                vectorSquaredL2: hit.has_vector_score == 1 ? hit.vector_squared_l2 : nil,
                lexicalBM25: hit.has_lexical_score == 1 ? hit.lexical_bm25 : nil
            )
        }
    }

    private nonisolated static func withEpochRequest<Result>(
        _ epoch: Epoch,
        _ body: (UnsafePointer<ZeEpochRequest>) throws -> Result
    ) rethrows -> Result {
        let fields: [[UInt8]] = [
            Array(epoch.embedding.document.modelID.utf8),
            Array(epoch.embedding.document.modelVersion.utf8),
            Array(epoch.embedding.document.weightsDigest),
            Array(epoch.embedding.document.promptPrefix.utf8),
            epoch.embedding.document.operatingSystemBuild.map { Array($0.utf8) } ?? [],
            Array(epoch.embedding.query.modelID.utf8),
            Array(epoch.embedding.query.modelVersion.utf8),
            Array(epoch.embedding.query.weightsDigest),
            Array(epoch.embedding.query.promptPrefix.utf8),
            epoch.embedding.query.operatingSystemBuild.map { Array($0.utf8) } ?? [],
            Array(epoch.embedding.alignmentDigest),
        ]
        let offsets = offsets(fields.map(\.count))
        let bytes = fields.flatMap { $0 }
        return try bytes.withUnsafeBufferPointer { buffer in
            func bytesPointer(_ index: Int) -> UnsafePointer<UInt8>? {
                pointer(buffer.baseAddress, offset: offsets[index], count: fields[index].count)
            }
            func tower(_ value: EmbeddingTower, base: Int) -> ZeEmbeddingTower {
                var tower = ZeEmbeddingTower()
                tower.model_id = bytesPointer(base)
                tower.model_id_len = fields[base].count
                tower.model_version = bytesPointer(base + 1)
                tower.model_version_len = fields[base + 1].count
                tower.weights_digest = bytesPointer(base + 2)
                tower.weights_digest_len = fields[base + 2].count
                tower.dims = value.dimensions
                tower.normalization = value.normalization.rawValue
                tower.prompt_prefix = bytesPointer(base + 3)
                tower.prompt_prefix_len = fields[base + 3].count
                tower.max_tokens = value.maxTokens
                tower.runtime = value.runtime.rawValue
                tower.compute_units = value.computeUnits.rawValue
                tower.has_os_build = value.operatingSystemBuild == nil ? 0 : 1
                tower.os_build = bytesPointer(base + 4)
                tower.os_build_len = fields[base + 4].count
                return tower
            }
            var request = ZeEpochRequest()
            request.abi_size = abiSize(ZeEpochRequest.self)
            request.embedding.document = tower(epoch.embedding.document, base: 0)
            request.embedding.query = tower(epoch.embedding.query, base: 5)
            request.embedding.alignment_digest = bytesPointer(10)
            request.embedding.alignment_digest_len = fields[10].count
            request.tokenizer_profile = 0
            return try withUnsafePointer(to: &request, body)
        }
    }
}
