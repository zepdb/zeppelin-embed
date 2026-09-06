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
        XCTAssertEqual(ZeppelinError.allCases.map(\.rawValue), Array(0...34))
    }

    func testSwiftMatchesRustCrossBindingParityFixture() async throws {
        let fixtureURL = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("bindings/fixtures/cross_binding_parity_v1.json")
        let fixtureData: Data
        do {
            fixtureData = try Data(contentsOf: fixtureURL)
        } catch {
            XCTFail("operation fixture_load: missing fixture at \(fixtureURL.path): \(error)")
            return
        }

        let fixture: ParityFixture
        do {
            fixture = try JSONDecoder().decode(ParityFixture.self, from: fixtureData)
        } catch {
            XCTFail("operation fixture_decode: could not parse fixture: \(error)")
            return
        }

        XCTAssertEqual(
            fixture.schema,
            "zeppelin-embed-cross-binding-parity",
            "operation fixture_header: schema"
        )
        XCTAssertEqual(fixture.version, 1, "operation fixture_header: version")
        XCTAssertEqual(fixture.seed, 25_172_023, "operation fixture_header: seed")
        XCTAssertEqual(fixture.scorePrecision, 6, "operation fixture_header: score precision")

        let epoch: Epoch
        do {
            XCTAssertEqual(
                fixture.epoch.tokenizerProfile,
                0,
                "operation epoch_identity: tokenizer profile"
            )
            epoch = Epoch(
                embedding: EmbeddingEpoch(
                    document: try parityTower(
                        fixture.epoch.document,
                        operation: "epoch_identity.document"
                    ),
                    query: try parityTower(
                        fixture.epoch.query,
                        operation: "epoch_identity.query"
                    ),
                    alignmentDigest: try parityData(
                        hex: fixture.epoch.alignmentDigestHex,
                        operation: "epoch_identity.alignment_digest_hex"
                    )
                )
            )
        } catch {
            XCTFail("operation epoch_identity: invalid epoch fixture: \(error)")
            return
        }

        let expectedIdentity = EpochIdentity(
            embeddingEpoch: fixture.epoch.expectedIdentity.embeddingEpoch,
            tokenizerEpoch: fixture.epoch.expectedIdentity.tokenizerEpoch
        )
        do {
            let identity = try await ZeppelinStore.epochIdentity(epoch)
            XCTAssertEqual(
                identity,
                expectedIdentity,
                "operation epoch_identity: identity"
            )
        } catch {
            XCTFail("operation epoch_identity failed: \(error)")
            return
        }

        let path = try storePath(#function)
        defer { try? FileManager.default.removeItem(at: path) }
        let namespaceParent = try storePath("\(#function)-namespaces")
        defer { try? FileManager.default.removeItem(at: namespaceParent) }
        var store: ZeppelinStore?
        var namespaceStore: ZeppelinStore?
        var namespaceRoot: URL?
        var namespaceDefinitions: [String: ParityAttributeDefinition] = [:]

        for operation in fixture.operations {
            do {
                switch operation {
                case .openWithEpoch(let expected):
                    XCTAssertNil(store, "operation \(operation.label): store already open")
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    store = try await ZeppelinStore.openWithEpoch(at: path, epoch: epoch)

                case .ingest(let documents, let expected):
                    guard let store else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let records = try documents.map { document in
                        IngestDocument(
                            id: DocumentID(
                                high: document.docID.high,
                                low: document.docID.low
                            ),
                            revision: document.revision,
                            timestamp: document.timestamp,
                            vector: document.vector,
                            text: document.text,
                            metadata: try parityData(
                                hex: document.metadataHex,
                                operation: operation.label
                            )
                        )
                    }
                    let report = try await store.ingest(records)
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        report.sequence,
                        expected.sequence,
                        "operation \(operation.label): sequence"
                    )
                    XCTAssertEqual(
                        report.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )

                case .query(let name, let request, let expected):
                    guard let store else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let result = try await store.query(
                        vector: request.vector.value,
                        text: request.text.value,
                        options: QueryOptions(
                            k: request.k,
                            threadBudget: 1,
                            tier: try parityTier(request.tier.value, operation: operation.label),
                            rulesEnabled: request.rulesEnabled
                        )
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        result.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )
                    XCTAssertEqual(
                        result.mode.rawValue,
                        expected.mode,
                        "operation \(operation.label): mode"
                    )
                    XCTAssertEqual(
                        result.diagnostics.embeddingEpoch,
                        expected.embeddingEpoch.value,
                        "operation \(operation.label): embedding epoch"
                    )
                    XCTAssertEqual(
                        result.diagnostics.tokenizerEpoch,
                        expected.tokenizerEpoch.value,
                        "operation \(operation.label): tokenizer epoch"
                    )
                    XCTAssertEqual(
                        result.hits.count,
                        expected.hits.count,
                        "operation \(operation.label): hit count"
                    )
                    for index in 0..<min(result.hits.count, expected.hits.count) {
                        let hit = result.hits[index]
                        let expectedHit = expected.hits[index]
                        XCTAssertEqual(
                            hit.documentID,
                            DocumentID(
                                high: expectedHit.docID.high,
                                low: expectedHit.docID.low
                            ),
                            "operation query \(name) hit \(index): document id"
                        )
                        XCTAssertEqual(
                            parityRounded(hit.score, precision: fixture.scorePrecision),
                            expectedHit.score,
                            "operation query \(name) hit \(index): score"
                        )
                        XCTAssertEqual(
                            hit.vectorSquaredL2.map {
                                parityRounded($0, precision: fixture.scorePrecision)
                            },
                            expectedHit.vectorSquaredL2.value,
                            "operation query \(name) hit \(index): vector_squared_l2"
                        )
                        XCTAssertEqual(
                            hit.lexicalBM25.map {
                                parityRounded($0, precision: fixture.scorePrecision)
                            },
                            expectedHit.lexicalBM25.value,
                            "operation query \(name) hit \(index): lexical_bm25"
                        )
                    }

                case .invalidEmptyQuery(let request, let expected):
                    guard let store else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    do {
                        _ = try await store.query(
                            vector: request.vector.value,
                            text: request.text.value,
                            options: QueryOptions(k: 4)
                        )
                        XCTFail("operation \(operation.label): query unexpectedly succeeded")
                    } catch let error as ZeppelinError {
                        XCTAssertEqual(
                            try error.codeName,
                            expected.errorCode,
                            "operation \(operation.label): exact C error name"
                        )
                    }
                    let currentEpoch = try await store.currentEpoch()
                    XCTAssertEqual(
                        currentEpoch,
                        expectedIdentity,
                        "operation \(operation.label): current epoch"
                    )

                case .close(let expected):
                    guard let current = store else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    try await current.close()
                    store = nil

                case .namespaceOpen(let root, let name, let spec, let expected):
                    XCTAssertNil(
                        namespaceStore,
                        "operation \(operation.label): namespace already open"
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    var definitions: [AttributeDefinition] = []
                    for attribute in spec.attributes {
                        guard let type = AttributeType(rawValue: attribute.attributeType) else {
                            throw ParityFixtureError.invalidValue(
                                operation: operation.label,
                                field: "attribute_type",
                                value: String(attribute.attributeType)
                            )
                        }
                        definitions.append(
                            AttributeDefinition(
                                id: attribute.attributeID,
                                name: attribute.name,
                                type: type,
                                nullable: attribute.nullable
                            )
                        )
                        namespaceDefinitions[attribute.name] = attribute
                    }
                    var vectorSpace: VectorSpace?
                    if let space = spec.vectorSpace {
                        guard
                            let normalization = VectorNormalization(
                                rawValue: space.normalization
                            )
                        else {
                            throw ParityFixtureError.invalidValue(
                                operation: operation.label,
                                field: "normalization",
                                value: String(space.normalization)
                            )
                        }
                        vectorSpace = VectorSpace(
                            dimensions: space.dimensions,
                            normalization: normalization
                        )
                    }
                    let rootURL = namespaceParent
                        .appendingPathComponent(root, isDirectory: true)
                    try FileManager.default.createDirectory(
                        at: rootURL,
                        withIntermediateDirectories: true
                    )
                    namespaceRoot = rootURL
                    namespaceStore = try await ZeppelinStore.openNamespace(
                        root: rootURL,
                        name: name,
                        spec: NamespaceSpec(
                            attributes: definitions,
                            vectorSpace: vectorSpace
                        )
                    )

                case .namespaceList(let root, let expected):
                    guard let rootURL = namespaceRoot else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    XCTAssertEqual(
                        rootURL.lastPathComponent,
                        root,
                        "operation \(operation.label): root"
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    let names = try await ZeppelinStore.listNamespaces(root: rootURL)
                    XCTAssertEqual(
                        names,
                        expected.names,
                        "operation \(operation.label): names"
                    )

                case .upsert(let documents, let expected):
                    guard let namespaceStore else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let records = try documents.map { document in
                        IngestDocument(
                            id: DocumentID(
                                high: document.docID.high,
                                low: document.docID.low
                            ),
                            revision: document.revision,
                            timestamp: document.timestamp,
                            vector: document.vector,
                            text: document.text,
                            metadata: try parityData(
                                hex: document.metadataHex,
                                operation: operation.label
                            ),
                            attributes: try parityAttributes(
                                document.attributes,
                                operation: operation.label
                            )
                        )
                    }
                    let report = try await namespaceStore.upsert(records)
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        report.sequence,
                        expected.sequence,
                        "operation \(operation.label): sequence"
                    )
                    XCTAssertEqual(
                        report.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )

                case .get(let ids, let expected):
                    guard let namespaceStore else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let result = try await namespaceStore.get(
                        ids.map { DocumentID(high: $0.high, low: $0.low) }
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        result.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )
                    XCTAssertEqual(
                        result.missingCount,
                        expected.missingCount,
                        "operation \(operation.label): missing count"
                    )
                    XCTAssertEqual(
                        result.documents.count,
                        expected.documents.count,
                        "operation \(operation.label): document count"
                    )
                    for index in 0..<min(result.documents.count, expected.documents.count) {
                        let document = result.documents[index]
                        guard let expectedDocument = expected.documents[index] else {
                            XCTAssertNil(
                                document,
                                "operation \(operation.label) document \(index): expected missing"
                            )
                            continue
                        }
                        guard let document else {
                            XCTFail(
                                "operation \(operation.label) document \(index): unexpectedly missing"
                            )
                            continue
                        }
                        XCTAssertEqual(
                            document.id,
                            DocumentID(
                                high: expectedDocument.docID.high,
                                low: expectedDocument.docID.low
                            ),
                            "operation \(operation.label) document \(index): document id"
                        )
                        XCTAssertEqual(
                            document.revision,
                            expectedDocument.revision,
                            "operation \(operation.label) document \(index): revision"
                        )
                        XCTAssertEqual(
                            document.timestamp,
                            expectedDocument.timestamp,
                            "operation \(operation.label) document \(index): timestamp"
                        )
                        XCTAssertEqual(
                            document.vector,
                            expectedDocument.vector,
                            "operation \(operation.label) document \(index): vector"
                        )
                        XCTAssertEqual(
                            document.text,
                            expectedDocument.text,
                            "operation \(operation.label) document \(index): text"
                        )
                        XCTAssertEqual(
                            document.metadata,
                            try parityData(
                                hex: expectedDocument.metadataHex,
                                operation: operation.label
                            ),
                            "operation \(operation.label) document \(index): metadata"
                        )
                        XCTAssertEqual(
                            document.attributes,
                            try parityAttributes(
                                expectedDocument.attributes,
                                operation: operation.label
                            ),
                            "operation \(operation.label) document \(index): attributes"
                        )
                    }

                case .scan(let request, let expected):
                    guard let namespaceStore else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let page = try await namespaceStore.scan(
                        limit: request.limit,
                        order: try parityScanOrder(
                            request.order,
                            operation: operation.label
                        )
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        page.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )
                    XCTAssertEqual(
                        page.next != nil,
                        expected.hasMore,
                        "operation \(operation.label): has more"
                    )
                    XCTAssertEqual(
                        page.documents.map(\.id.high),
                        Array(repeating: 0, count: expected.docIDs.count),
                        "operation \(operation.label): document id high halves"
                    )
                    XCTAssertEqual(
                        page.documents.map(\.id.low),
                        expected.docIDs,
                        "operation \(operation.label): document ids"
                    )

                case .count(let filter, let expected):
                    guard let namespaceStore else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let result = try await namespaceStore.count(
                        filter: try parityFilter(
                            filter,
                            definitions: namespaceDefinitions,
                            operation: operation.label
                        )
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        result.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )
                    XCTAssertEqual(
                        result.count,
                        expected.count,
                        "operation \(operation.label): count"
                    )

                case .searchFiltered(let request, let expected):
                    guard let namespaceStore else {
                        throw ParityFixtureError.missingStore(operation: operation.label)
                    }
                    let result = try await namespaceStore.search(
                        vector: request.vector,
                        filter: try parityFilter(
                            request.filter,
                            definitions: namespaceDefinitions,
                            operation: operation.label
                        ),
                        options: SearchOptions(
                            k: request.k,
                            threadBudget: 1,
                            tier: try parityTier(
                                request.tier,
                                operation: operation.label
                            )
                        )
                    )
                    XCTAssertEqual(
                        expected.errorCode,
                        try ZeppelinError.ok.codeName,
                        "operation \(operation.label): error code"
                    )
                    XCTAssertEqual(
                        result.generation,
                        expected.generation,
                        "operation \(operation.label): generation"
                    )
                    XCTAssertEqual(
                        result.hits.count,
                        expected.hits.count,
                        "operation \(operation.label): hit count"
                    )
                    for index in 0..<min(result.hits.count, expected.hits.count) {
                        let hit = result.hits[index]
                        let expectedHit = expected.hits[index]
                        XCTAssertEqual(
                            hit.documentID,
                            DocumentID(
                                high: expectedHit.docID.high,
                                low: expectedHit.docID.low
                            ),
                            "operation \(operation.label) hit \(index): document id"
                        )
                        XCTAssertEqual(
                            parityRounded(
                                Double(hit.score),
                                precision: fixture.scorePrecision
                            ),
                            expectedHit.score,
                            "operation \(operation.label) hit \(index): score"
                        )
                    }
                }
            } catch {
                XCTFail("operation \(operation.label) failed: \(error)")
                if let store {
                    try? await store.close()
                }
                if let namespaceStore {
                    try? await namespaceStore.close()
                }
                return
            }
        }

        XCTAssertNil(store, "operation fixture_complete: close was not replayed")
        XCTAssertNotNil(
            namespaceStore,
            "operation fixture_complete: the record store operations were not replayed"
        )
        if let namespaceStore {
            try await namespaceStore.close()
        }
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

    private func parityTower(_ value: ParityTower, operation: String) throws -> EmbeddingTower {
        guard let normalization = VectorNormalization(rawValue: value.normalization) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "normalization",
                value: String(value.normalization)
            )
        }
        guard let runtime = EmbeddingRuntime(rawValue: value.runtime) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "runtime",
                value: String(value.runtime)
            )
        }
        guard let computeUnits = ComputeUnits(rawValue: value.computeUnits) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "compute_units",
                value: String(value.computeUnits)
            )
        }
        return EmbeddingTower(
            modelID: value.modelID,
            modelVersion: value.modelVersion,
            weightsDigest: try parityData(hex: value.weightsDigestHex, operation: operation),
            dimensions: value.dimensions,
            normalization: normalization,
            promptPrefix: value.promptPrefix,
            maxTokens: value.maxTokens,
            runtime: runtime,
            computeUnits: computeUnits,
            operatingSystemBuild: value.operatingSystemBuild.value
        )
    }

    private func parityData(hex: String, operation: String) throws -> Data {
        guard hex.count.isMultiple(of: 2) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "hex",
                value: hex
            )
        }
        var bytes: [UInt8] = []
        bytes.reserveCapacity(hex.count / 2)
        var start = hex.startIndex
        while start < hex.endIndex {
            let end = hex.index(start, offsetBy: 2)
            guard let byte = UInt8(hex[start..<end], radix: 16) else {
                throw ParityFixtureError.invalidValue(
                    operation: operation,
                    field: "hex",
                    value: hex
                )
            }
            bytes.append(byte)
            start = end
        }
        return Data(bytes)
    }

    private func parityTier(_ rawValue: Int32?, operation: String) throws -> SearchTier? {
        guard let rawValue else {
            return nil
        }
        guard let tier = SearchTier(rawValue: rawValue) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "tier",
                value: String(rawValue)
            )
        }
        return tier
    }

    private func parityAttributeValue(
        rawType: Int32,
        scalar: ParityScalar,
        operation: String
    ) throws -> AttributeValue {
        guard let type = AttributeType(rawValue: rawType) else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "attribute_type",
                value: String(rawType)
            )
        }
        switch (type, scalar) {
        case (_, .null):
            return .null
        case (.u64, .unsignedInteger(let value)):
            return .u64(value)
        case (.i64, .unsignedInteger(let value)):
            return .i64(Int64(value))
        case (.i64, .signedInteger(let value)):
            return .i64(value)
        case (.f64, .unsignedInteger(let value)):
            return .f64(Double(value))
        case (.f64, .signedInteger(let value)):
            return .f64(Double(value))
        case (.f64, .number(let value)):
            return .f64(value)
        case (.bool, .bool(let value)):
            return .bool(value)
        case (.dictionaryString, .string(let value)):
            return .string(value)
        case (.rawString, .string(let value)):
            return .string(value)
        default:
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "attribute value",
                value: String(describing: scalar)
            )
        }
    }

    private func parityAttributes(
        _ values: [ParityAttributeValue],
        operation: String
    ) throws -> [UInt32: AttributeValue] {
        var attributes: [UInt32: AttributeValue] = [:]
        for value in values {
            attributes[value.attributeID] = try parityAttributeValue(
                rawType: value.attributeType,
                scalar: value.value,
                operation: operation
            )
        }
        return attributes
    }

    private func parityFilter(
        _ filter: ParityFilter,
        definitions: [String: ParityAttributeDefinition],
        operation: String
    ) throws -> Filter {
        guard let definition = definitions[filter.field] else {
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "filter field",
                value: filter.field
            )
        }
        let value = try parityAttributeValue(
            rawType: definition.attributeType,
            scalar: filter.value,
            operation: operation
        )
        switch filter.op {
        case "eq":
            return .eq(definition.attributeID, value)
        case "not_eq":
            return .notEq(definition.attributeID, value)
        default:
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "filter op",
                value: filter.op
            )
        }
    }

    private func parityScanOrder(_ value: String, operation: String) throws -> ScanOrder {
        switch value {
        case "storage":
            return .storage
        case "timestamp_ascending":
            return .timestampAscending
        case "timestamp_descending":
            return .timestampDescending
        default:
            throw ParityFixtureError.invalidValue(
                operation: operation,
                field: "order",
                value: value
            )
        }
    }

    private func parityRounded(_ value: Double, precision: Int) -> Double {
        let scale = pow(10, Double(precision))
        return (value * scale).rounded() / scale
    }

    private func storePath(_ name: String) throws -> URL {
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".build/test-stores", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root.appendingPathComponent("\(name)-\(UUID().uuidString)", isDirectory: true)
    }
}

private struct ParityFixture: Decodable {
    let schema: String
    let version: Int
    let seed: UInt64
    let scorePrecision: Int
    let epoch: ParityEpoch
    let operations: [ParityOperation]

    private enum CodingKeys: String, CodingKey {
        case schema
        case version
        case seed
        case scorePrecision = "score_precision"
        case epoch
        case operations
    }
}

private struct ParityEpoch: Decodable {
    let document: ParityTower
    let query: ParityTower
    let alignmentDigestHex: String
    let tokenizerProfile: Int32
    let expectedIdentity: ParityEpochIdentity

    private enum CodingKeys: String, CodingKey {
        case document
        case query
        case alignmentDigestHex = "alignment_digest_hex"
        case tokenizerProfile = "tokenizer_profile"
        case expectedIdentity = "expected_identity"
    }
}

private struct ParityTower: Decodable {
    let modelID: String
    let modelVersion: String
    let weightsDigestHex: String
    let dimensions: UInt32
    let normalization: Int32
    let promptPrefix: String
    let maxTokens: UInt32
    let runtime: Int32
    let computeUnits: Int32
    let operatingSystemBuild: ParityNullable<String>

    private enum CodingKeys: String, CodingKey {
        case modelID = "model_id"
        case modelVersion = "model_version"
        case weightsDigestHex = "weights_digest_hex"
        case dimensions = "dims"
        case normalization
        case promptPrefix = "prompt_prefix"
        case maxTokens = "max_tokens"
        case runtime
        case computeUnits = "compute_units"
        case operatingSystemBuild = "os_build"
    }
}

private struct ParityEpochIdentity: Decodable {
    let embeddingEpoch: UInt64
    let tokenizerEpoch: UInt64

    private enum CodingKeys: String, CodingKey {
        case embeddingEpoch = "embedding_epoch"
        case tokenizerEpoch = "tokenizer_epoch"
    }
}

private struct ParityDocumentID: Decodable {
    let high: UInt64
    let low: UInt64
}

private struct ParityDocument: Decodable {
    let docID: ParityDocumentID
    let revision: UInt64
    let timestamp: Int64
    let vector: [Float]
    let text: String
    let metadataHex: String

    private enum CodingKeys: String, CodingKey {
        case docID = "doc_id"
        case revision
        case timestamp
        case vector
        case text
        case metadataHex = "metadata_hex"
    }
}

private struct ParityErrorExpectation: Decodable {
    let errorCode: String

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
    }
}

private struct ParityMutationExpectation: Decodable {
    let errorCode: String
    let sequence: UInt64
    let generation: UInt64

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case sequence
        case generation
    }
}

private struct ParityQueryRequest: Decodable {
    let vector: ParityNullable<[Float]>
    let text: ParityNullable<String>
    let k: Int
    let tier: ParityNullable<Int32>
    let rulesEnabled: Bool

    private enum CodingKeys: String, CodingKey {
        case vector
        case text
        case k
        case tier
        case rulesEnabled = "rules_enabled"
    }
}

private struct ParityEmptyQueryRequest: Decodable {
    let vector: ParityNullable<[Float]>
    let text: ParityNullable<String>
}

private struct ParityQueryExpectation: Decodable {
    let errorCode: String
    let generation: UInt64
    let mode: Int32
    let embeddingEpoch: ParityNullable<UInt64>
    let tokenizerEpoch: ParityNullable<UInt64>
    let hits: [ParityQueryHit]

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case generation
        case mode
        case embeddingEpoch = "embedding_epoch"
        case tokenizerEpoch = "tokenizer_epoch"
        case hits
    }
}

private struct ParityQueryHit: Decodable {
    let docID: ParityDocumentID
    let score: Double
    let vectorSquaredL2: ParityNullable<Double>
    let lexicalBM25: ParityNullable<Double>

    private enum CodingKeys: String, CodingKey {
        case docID = "doc_id"
        case score
        case vectorSquaredL2 = "vector_squared_l2"
        case lexicalBM25 = "lexical_bm25"
    }
}

private enum ParityScalar: Decodable {
    case null
    case bool(Bool)
    case unsignedInteger(UInt64)
    case signedInteger(Int64)
    case number(Double)
    case string(String)

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(UInt64.self) {
            self = .unsignedInteger(value)
        } else if let value = try? container.decode(Int64.self) {
            self = .signedInteger(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else {
            self = .string(try container.decode(String.self))
        }
    }
}

private struct ParityAttributeDefinition: Decodable {
    let attributeID: UInt32
    let name: String
    let attributeType: Int32
    let nullable: Bool

    private enum CodingKeys: String, CodingKey {
        case attributeID = "attribute_id"
        case name
        case attributeType = "attribute_type"
        case nullable
    }
}

private struct ParityVectorSpace: Decodable {
    let dimensions: UInt32
    let normalization: Int32
}

private struct ParityNamespaceSpec: Decodable {
    let attributes: [ParityAttributeDefinition]
    let vectorSpace: ParityVectorSpace?

    private enum CodingKeys: String, CodingKey {
        case attributes
        case vectorSpace = "vector_space"
    }
}

private struct ParityAttributeValue: Decodable {
    let attributeID: UInt32
    let attributeType: Int32
    let value: ParityScalar

    private enum CodingKeys: String, CodingKey {
        case attributeID = "attribute_id"
        case attributeType = "attribute_type"
        case value
    }
}

private struct ParityStoredDocument: Decodable {
    let docID: ParityDocumentID
    let revision: UInt64
    let timestamp: Int64
    let vector: [Float]
    let text: String
    let metadataHex: String
    let attributes: [ParityAttributeValue]

    private enum CodingKeys: String, CodingKey {
        case docID = "doc_id"
        case revision
        case timestamp
        case vector
        case text
        case metadataHex = "metadata_hex"
        case attributes
    }
}

private struct ParityNamespaceListExpectation: Decodable {
    let errorCode: String
    let names: [String]

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case names
    }
}

private struct ParityGetExpectation: Decodable {
    let errorCode: String
    let generation: UInt64
    let missingCount: Int
    let documents: [ParityStoredDocument?]

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case generation
        case missingCount = "missing_count"
        case documents
    }
}

private struct ParityScanRequest: Decodable {
    let order: String
    let limit: Int
}

private struct ParityScanExpectation: Decodable {
    let errorCode: String
    let generation: UInt64
    let hasMore: Bool
    let docIDs: [UInt64]

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case generation
        case hasMore = "has_more"
        case docIDs = "doc_ids"
    }
}

private struct ParityFilter: Decodable {
    let op: String
    let field: String
    let value: ParityScalar
}

private struct ParityCountExpectation: Decodable {
    let errorCode: String
    let generation: UInt64
    let count: UInt64

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case generation
        case count
    }
}

private struct ParitySearchFilteredRequest: Decodable {
    let vector: [Float]
    let k: Int
    let tier: Int32
    let filter: ParityFilter
}

private struct ParitySearchHit: Decodable {
    let docID: ParityDocumentID
    let score: Double

    private enum CodingKeys: String, CodingKey {
        case docID = "doc_id"
        case score
    }
}

private struct ParitySearchFilteredExpectation: Decodable {
    let errorCode: String
    let generation: UInt64
    let hits: [ParitySearchHit]

    private enum CodingKeys: String, CodingKey {
        case errorCode = "error_code"
        case generation
        case hits
    }
}

private enum ParityOperation: Decodable {
    case openWithEpoch(ParityErrorExpectation)
    case ingest([ParityDocument], ParityMutationExpectation)
    case query(String, ParityQueryRequest, ParityQueryExpectation)
    case invalidEmptyQuery(ParityEmptyQueryRequest, ParityErrorExpectation)
    case close(ParityErrorExpectation)
    case namespaceOpen(String, String, ParityNamespaceSpec, ParityErrorExpectation)
    case namespaceList(String, ParityNamespaceListExpectation)
    case upsert([ParityStoredDocument], ParityMutationExpectation)
    case get([ParityDocumentID], ParityGetExpectation)
    case scan(ParityScanRequest, ParityScanExpectation)
    case count(ParityFilter, ParityCountExpectation)
    case searchFiltered(ParitySearchFilteredRequest, ParitySearchFilteredExpectation)

    var label: String {
        switch self {
        case .openWithEpoch:
            "open_with_epoch"
        case .ingest:
            "ingest"
        case .query(let name, _, _):
            "query \(name)"
        case .invalidEmptyQuery:
            "invalid_empty_query"
        case .close:
            "close"
        case .namespaceOpen:
            "namespace_open"
        case .namespaceList:
            "namespace_list"
        case .upsert:
            "upsert"
        case .get:
            "get"
        case .scan:
            "scan"
        case .count:
            "count"
        case .searchFiltered:
            "search_filtered"
        }
    }

    private enum CodingKeys: String, CodingKey {
        case kind
        case name
        case documents
        case request
        case expected
        case root
        case spec
        case ids
        case filter
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try container.decode(String.self, forKey: .kind)
        let label = try container.decodeIfPresent(String.self, forKey: .name) ?? kind
        do {
            switch kind {
            case "open_with_epoch":
                self = .openWithEpoch(
                    try container.decode(ParityErrorExpectation.self, forKey: .expected)
                )
            case "ingest":
                self = .ingest(
                    try container.decode([ParityDocument].self, forKey: .documents),
                    try container.decode(ParityMutationExpectation.self, forKey: .expected)
                )
            case "query":
                self = .query(
                    try container.decode(String.self, forKey: .name),
                    try container.decode(ParityQueryRequest.self, forKey: .request),
                    try container.decode(ParityQueryExpectation.self, forKey: .expected)
                )
            case "invalid_empty_query":
                self = .invalidEmptyQuery(
                    try container.decode(ParityEmptyQueryRequest.self, forKey: .request),
                    try container.decode(ParityErrorExpectation.self, forKey: .expected)
                )
            case "close":
                self = .close(
                    try container.decode(ParityErrorExpectation.self, forKey: .expected)
                )
            case "namespace_open":
                self = .namespaceOpen(
                    try container.decode(String.self, forKey: .root),
                    try container.decode(String.self, forKey: .name),
                    try container.decode(ParityNamespaceSpec.self, forKey: .spec),
                    try container.decode(ParityErrorExpectation.self, forKey: .expected)
                )
            case "namespace_list":
                self = .namespaceList(
                    try container.decode(String.self, forKey: .root),
                    try container.decode(
                        ParityNamespaceListExpectation.self,
                        forKey: .expected
                    )
                )
            case "upsert":
                self = .upsert(
                    try container.decode([ParityStoredDocument].self, forKey: .documents),
                    try container.decode(ParityMutationExpectation.self, forKey: .expected)
                )
            case "get":
                self = .get(
                    try container.decode([ParityDocumentID].self, forKey: .ids),
                    try container.decode(ParityGetExpectation.self, forKey: .expected)
                )
            case "scan":
                self = .scan(
                    try container.decode(ParityScanRequest.self, forKey: .request),
                    try container.decode(ParityScanExpectation.self, forKey: .expected)
                )
            case "count":
                self = .count(
                    try container.decode(ParityFilter.self, forKey: .filter),
                    try container.decode(ParityCountExpectation.self, forKey: .expected)
                )
            case "search_filtered":
                self = .searchFiltered(
                    try container.decode(
                        ParitySearchFilteredRequest.self,
                        forKey: .request
                    ),
                    try container.decode(
                        ParitySearchFilteredExpectation.self,
                        forKey: .expected
                    )
                )
            default:
                throw DecodingError.dataCorruptedError(
                    forKey: .kind,
                    in: container,
                    debugDescription: "operation \(label): unknown kind \(kind)"
                )
            }
        } catch {
            throw DecodingError.dataCorrupted(
                DecodingError.Context(
                    codingPath: decoder.codingPath,
                    debugDescription: "operation \(label): \(error)"
                )
            )
        }
    }
}

private enum ParityNullable<Value: Decodable>: Decodable {
    case null
    case present(Value)

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else {
            self = .present(try container.decode(Value.self))
        }
    }

    var value: Value? {
        switch self {
        case .null:
            nil
        case .present(let value):
            value
        }
    }
}

private enum ParityFixtureError: LocalizedError {
    case invalidValue(operation: String, field: String, value: String)
    case missingStore(operation: String)

    var errorDescription: String? {
        switch self {
        case .invalidValue(let operation, let field, let value):
            "operation \(operation): invalid \(field) value \(value)"
        case .missingStore(let operation):
            "operation \(operation): store is not open"
        }
    }
}

private extension UInt64 {
    func absDiff(_ other: UInt64) -> UInt64 {
        self >= other ? self - other : other - self
    }
}
