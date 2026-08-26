import Foundation
import XCTest
#if canImport(Darwin)
import Darwin
#endif

@_spi(Testing) @testable import ZeppelinEmbed

final class ZeppelinStoreTests: XCTestCase {
    func testOpenIngestQueryCloseRoundTripSucceeds() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let documents = [
            IngestDocument(
                id: DocumentID(high: 0, low: 1),
                revision: 1,
                timestamp: 1,
                vector: vector(0),
                text: "zeppelin airship over the harbour"
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 2),
                revision: 1,
                timestamp: 2,
                vector: vector(1),
                text: "harbour lights at dusk"
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 3),
                revision: 1,
                timestamp: 3,
                vector: vector(2),
                text: "quantized vectors and postings"
            ),
        ]

        let mutation = try await store.ingest(documents)
        XCTAssertGreaterThan(mutation.sequence, 0)
        let result = try await store.query(
            vector: vector(1),
            text: "harbour",
            options: QueryOptions(k: 3)
        )
        XCTAssertEqual(result.mode, .hybrid)
        XCTAssertEqual(result.hits.count, 3)
        XCTAssertTrue(result.diagnostics.exactRescore)
        XCTAssertTrue(result.hits.contains { $0.lexicalBM25 != nil })
        XCTAssertTrue(result.hits.allSatisfy { $0.vectorSquaredL2 != nil })

        try await store.close()
        try await store.close()
        do {
            _ = try await store.state()
            XCTFail("use after close succeeded")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .closed)
        }
    }

    func testTenThousandQueriesKeepPhysicalFootprintFlat() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        _ = try await store.ingest([
            IngestDocument(
                id: DocumentID(high: 0, low: 1),
                revision: 1,
                timestamp: 1,
                vector: vector(0),
                text: "zeppelin"
            ),
        ])
        let options = QueryOptions(k: 1)
        for _ in 0..<100 {
            _ = try await store.query(vector: vector(0), options: options)
        }
        guard let baseline = try await store.stats().physicalFootprint else {
            throw XCTSkip("phys_footprint is unavailable")
        }
        for _ in 0..<10_000 {
            let result = try await store.query(vector: vector(0), options: options)
            XCTAssertEqual(result.hits.count, 1)
        }
        let finalStats = try await store.stats()
        let final = try XCTUnwrap(finalStats.physicalFootprint)
        print(
            "SWIFT_FOOTPRINT iterations=10000 baseline_bytes=\(baseline) "
                + "final_bytes=\(final) drift_bytes=\(final.absDiff(baseline))"
        )
        XCTAssertLessThan(
            final.absDiff(baseline),
            8 * 1_024 * 1_024,
            "physical footprint drifted over 10,000 copied-and-freed query results"
        )
        try await store.close()
    }

    func testQuiesceAndResumeCloseAndReopenTheStore() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        _ = try await store.ingest([
            IngestDocument(
                id: DocumentID(high: 0, low: 1),
                revision: 1,
                timestamp: 1,
                vector: vector(0),
                text: "zeppelin"
            ),
        ])

        try await store.quiesce()
        do {
            _ = try await store.query(vector: vector(0), options: QueryOptions(k: 1))
            XCTFail("query succeeded while quiesced")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .closed)
        }

        try await store.resume()
        let result = try await store.query(vector: vector(0), options: QueryOptions(k: 1))
        XCTAssertEqual(result.hits.first?.documentID, DocumentID(high: 0, low: 1))
        try await store.close()
    }

    func testWithStoreClosesOnThrowAndReleasesThePath() async throws {
        enum Deliberate: Error { case stop }

        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        do {
            _ = try await ZeppelinStore.withStore(at: path) { _ in
                throw Deliberate.stop
            }
            XCTFail("withStore did not rethrow")
        } catch Deliberate.stop {
            // Expected; the lock must already have been released.
        }
        let reopened = try await ZeppelinStore.open(at: path)
        try await reopened.close()
    }

    func testEveryGeneratedErrorCodeHasATypedSwiftCase() {
        XCTAssertEqual(ZeppelinError.allCases.map(\.rawValue), Array(0...28))
    }

    func testDefaultOpenExcludesTheStoreFromBackup() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let attributeSize = path.path.withCString { fileSystemPath in
            getxattr(
                fileSystemPath,
                "com.apple.metadata:com_apple_backup_excludeItem",
                nil,
                0,
                0,
                0
            )
        }
        XCTAssertGreaterThan(attributeSize, 0)
        try await store.close()
    }

    func testCorruptManifestIsCatchableAndTheHostSurvives() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        try FileManager.default.createDirectory(at: path, withIntermediateDirectories: true)
        try Data("not a manifest".utf8).write(to: path.appendingPathComponent("manifest.ze"))
        do {
            _ = try await ZeppelinStore.open(at: path)
            XCTFail("corrupt manifest opened")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .corrupt)
        }
    }

    func testTenThousandConcurrentIngestsAreSerializedAndSearchable() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        try await withThrowingTaskGroup(of: Void.self) { group in
            for index in 1...10_000 {
                group.addTask {
                    _ = try await store.ingest([
                        IngestDocument(
                            id: DocumentID(high: 0, low: UInt64(index)),
                            revision: 1,
                            timestamp: Int64(index),
                            vector: [Float(index), 0, 0, 0]
                        ),
                    ])
                }
            }
            try await group.waitForAll()
        }
        let result = try await store.query(
            vector: [0, 0, 0, 0],
            options: QueryOptions(k: 10_000, tier: .exact)
        )
        XCTAssertEqual(Set(result.hits.compactMap(\.documentID)).count, 10_000)
        try await store.close()
    }

    func testConcurrentCloseAndQueryReturnOnlyTypedOutcomes() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let dimension = 8_192
        let vector = [Float](repeating: 0.25, count: dimension)
        let documents = (1...100).map { index in
            IngestDocument(
                id: DocumentID(high: 0, low: UInt64(index)),
                revision: 1,
                timestamp: Int64(index),
                vector: vector
            )
        }
        _ = try await store.ingest(documents)

        let query = Task { () -> Result<QueryResult, ZeppelinError> in
            do {
                return .success(
                    try await store.query(vector: vector, options: QueryOptions(k: 100))
                )
            } catch let error as ZeppelinError {
                return .failure(error)
            } catch {
                return .failure(.internalError)
            }
        }
        let close = Task { try await store.close() }
        let outcome = await query.value
        try await close.value
        switch outcome {
        case .success(let result):
            XCTAssertEqual(result.hits.count, 100)
        case .failure(let error):
            XCTAssertTrue([.closed, .closing, .cancelled].contains(error), "\(error)")
        }
    }

    func testCancellationMidQueryThrowsTypedCancelled() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let dimension = 8_192
        let vector = [Float](repeating: 0.25, count: dimension)
        _ = try await store.ingest((1...1_000).map { index in
            IngestDocument(
                id: DocumentID(high: 0, low: UInt64(index)),
                revision: 1,
                timestamp: Int64(index),
                vector: vector
            )
        })
        let token = try await ZeppelinCancellationToken.create()
        let (started, continuation) = AsyncStream<Void>.makeStream()
        let canceller = Task {
            for await _ in started {
                break
            }
            try token.cancelImmediatelyForTesting()
        }
        let query = Task {
            try await store.queryForCancellationTesting(
                vector: vector,
                options: QueryOptions(k: 1_000, threadBudget: 1, cancellationToken: token),
                onFFIEntry: {
                    continuation.yield(())
                    continuation.finish()
                }
            )
        }
        try await canceller.value
        do {
            _ = try await query.value
            XCTFail("query completed before cancellation")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .cancelled)
        }
        try await token.free()
        try await store.close()
    }

    func testABIPanicIsCaughtAndPoisonsTheSwiftStore() async throws {
        guard ProcessInfo.processInfo.environment["ZE_ABI_PANIC_PROBE"] == "1" else {
            throw XCTSkip("requires the abi-panic-probe static library")
        }
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        do {
            try await store.triggerABIPanicProbeForTesting()
            XCTFail("panic probe did not throw")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .panic)
        }
        do {
            _ = try await store.state()
            XCTFail("poisoned state call succeeded")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .poisoned)
        }
        do {
            try await store.close()
            XCTFail("poisoned close succeeded")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .poisoned)
        }
    }

    @MainActor
    func testAsyncQueryDoesNotBlockTheMainActorPastOneMillisecond() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        let vector = [Float](repeating: 0.25, count: 8_192)
        _ = try await store.ingest((1...1_000).map { index in
            IngestDocument(
                id: DocumentID(high: 0, low: UInt64(index)),
                revision: 1,
                timestamp: Int64(index),
                vector: vector
            )
        })
        let (entered, continuation) = AsyncStream<Void>.makeStream()
        let query = Task {
            try await store.queryForCancellationTesting(
                vector: vector,
                options: QueryOptions(k: 1_000, threadBudget: 1),
                onFFIEntry: {
                    continuation.yield(())
                    continuation.finish()
                }
            )
        }
        for await _ in entered {
            break
        }
        let clock = ContinuousClock()
        let start = clock.now
        let delay = await withCheckedContinuation { callback in
            DispatchQueue.main.async {
                callback.resume(returning: clock.now - start)
            }
        }
        print("MAIN_ACTOR_WATCHDOG delay=\(delay)")
        XCTAssertLessThan(delay, .milliseconds(1), "main-actor watchdog delay: \(delay)")
        _ = try await query.value
        try await store.close()
    }

    func testVectorParityUsesExactIDsScoresAndOrder() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let store = try await ZeppelinStore.open(at: path)
        _ = try await store.ingest([
            IngestDocument(
                id: DocumentID(high: 0, low: 1),
                revision: 1,
                timestamp: 1,
                vector: [0, 0, 0, 0]
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 2),
                revision: 1,
                timestamp: 2,
                vector: [1, 0, 0, 0]
            ),
        ])
        let result = try await store.query(
            vector: [0, 0, 0, 0],
            options: QueryOptions(k: 2, tier: .exact)
        )
        XCTAssertEqual(result.hits.map(\.documentID), [
            DocumentID(high: 0, low: 1),
            DocumentID(high: 0, low: 2),
        ])
        XCTAssertEqual(result.hits[0].score, 0, accuracy: 0.000_001)
        XCTAssertEqual(result.hits[1].score, -1, accuracy: 0.000_001)
        try await store.close()
    }

    func testEpochAndMaintenanceWrappersReachTheABI() async throws {
        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let tower = EmbeddingTower(
            modelID: "test-model",
            modelVersion: "1",
            weightsDigest: Data([1, 2, 3]),
            dimensions: 4,
            maxTokens: 32,
            runtime: .cpuReference,
            computeUnits: .cpu
        )
        let epoch = Epoch(embedding: EmbeddingEpoch(document: tower, query: tower))
        let expectedIdentity = try await ZeppelinStore.epochIdentity(epoch)
        let store = try await ZeppelinStore.openWithEpoch(at: path, epoch: epoch)
        let currentIdentity = try await store.currentEpoch()
        XCTAssertEqual(currentIdentity, expectedIdentity)
        _ = try await store.ingest([
            IngestDocument(
                id: DocumentID(high: 0, low: 1),
                revision: 1,
                timestamp: 1,
                vector: [0, 0, 0, 0],
                text: "zeppelin",
                metadata: Data([9, 8, 7])
            ),
            IngestDocument(
                id: DocumentID(high: 0, low: 2),
                revision: 1,
                timestamp: 2,
                vector: [1, 0, 0, 0]
            ),
        ])
        _ = try await store.delete([DocumentID(high: 0, low: 2)])
        let search = try await store.search(vector: [0, 0, 0, 0], options: SearchOptions(k: 1))
        XCTAssertEqual(search.hits.first?.documentID, DocumentID(high: 0, low: 1))
        _ = try await store.seal()
        let alias = try await store.switchAlias(to: epoch)
        XCTAssertEqual(alias.published, expectedIdentity)
        do {
            _ = try await store.dropEpoch(epoch)
            XCTFail("published epoch was dropped")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .epochPublished)
        }
        let partition = try await store.dropPartition(from: 100, to: 200)
        XCTAssertTrue(partition.isNoOp)
        let retention = try await store.applyRetention(window: 10, now: 0)
        XCTAssertTrue(retention.isNoOp)
        let purge = try await store.purge([DocumentID(high: .max, low: .max)])
        XCTAssertTrue(purge.isNoOp)
        let purged = try await store.awaitPhysicalPurge(purge)
        XCTAssertTrue(purged.isNoOp)
        let maintenance = try await store.maintain(wallTimeNanoseconds: 0, bytes: 0)
        XCTAssertEqual(maintenance.status, .budgetExhausted)
        try await store.close()
    }

    private func vector(_ seed: Int) -> [Float] {
        (0..<4).map { Float($0 + seed + 1) / 8 }
    }

    private func storePath(_ name: String) throws -> URL {
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".build/test-stores", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root.appendingPathComponent("\(name)-\(UUID().uuidString)", isDirectory: true)
    }
}

private extension UInt64 {
    func absDiff(_ other: UInt64) -> UInt64 {
        self >= other ? self - other : other - self
    }
}
