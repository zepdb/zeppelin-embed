import Foundation
import XCTest

@testable import ZeppelinEmbed

@available(macOS 14.0, *)
final class RecordStoreTests: XCTestCase {
    func testNamespaceCreateReopenSchemaMismatchAndList() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let spec = NamespaceSpec(
            attributes: [
                AttributeDefinition(id: 1, name: "category", type: .dictionaryString, nullable: true)
            ],
            vectorSpace: VectorSpace(dimensions: 2, normalization: .unitL2)
        )

        let created = try await ZeppelinStore.openNamespace(root: root, name: "records", spec: spec)
        try await created.close()
        let reopened = try await ZeppelinStore.openNamespace(root: root, name: "records", spec: spec)
        try await reopened.close()
        let namespaces = try await ZeppelinStore.listNamespaces(root: root)
        XCTAssertEqual(namespaces, ["records"])

        let changed = NamespaceSpec(
            attributes: [
                AttributeDefinition(id: 1, name: "language", type: .dictionaryString, nullable: true)
            ],
            vectorSpace: VectorSpace(dimensions: 2, normalization: .unitL2)
        )
        do {
            _ = try await ZeppelinStore.openNamespace(root: root, name: "records", spec: changed)
            XCTFail("namespace reopened with a different schema")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .schemaMismatch)
        }
    }

    func testUpsertEveryAttributeTypeRoundTripsThroughGet() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let definitions = [
            AttributeDefinition(id: 1, name: "u64", type: .u64, nullable: false),
            AttributeDefinition(id: 2, name: "i64", type: .i64, nullable: false),
            AttributeDefinition(id: 3, name: "f64", type: .f64, nullable: false),
            AttributeDefinition(id: 4, name: "bool", type: .bool, nullable: false),
            AttributeDefinition(id: 5, name: "dict", type: .dictionaryString, nullable: false),
            AttributeDefinition(id: 6, name: "raw", type: .rawString, nullable: false),
        ]
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "all-types",
            spec: NamespaceSpec(
                attributes: definitions,
                vectorSpace: VectorSpace(dimensions: 2, normalization: .unitL2)
            )
        )
        let id = DocumentID(high: 3, low: 5)
        let values: [UInt32: AttributeValue] = [
            1: .u64(42),
            2: .i64(-17),
            3: .f64(3.5),
            4: .bool(true),
            5: .string("alpha"),
            6: .string("bravo"),
        ]
        _ = try await store.upsert([
            IngestDocument(
                id: id,
                revision: 7,
                timestamp: -11,
                vector: [1, 0],
                text: "stored text",
                metadata: Data([7, 8, 9]),
                attributes: values
            )
        ])

        let result = try await store.get([id])
        let document = try XCTUnwrap(result.documents.first ?? nil)
        XCTAssertEqual(document.id, id)
        XCTAssertEqual(document.revision, 7)
        XCTAssertEqual(document.timestamp, -11)
        XCTAssertEqual(document.vector ?? [], [1, 0])
        XCTAssertEqual(document.text, "stored text")
        XCTAssertEqual(document.metadata, Data([7, 8, 9]))
        XCTAssertEqual(document.attributes, values)
        XCTAssertEqual(result.missingCount, 0)
        try await store.close()
    }

    func testGetPreservesPresentMissingTombstonedAndSupersededResults() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "get",
            spec: NamespaceSpec(
                attributes: [],
                vectorSpace: VectorSpace(dimensions: 1, normalization: .none)
            )
        )
        let superseded = DocumentID(high: 0, low: 1)
        let tombstoned = DocumentID(high: 0, low: 2)
        let missing = DocumentID(high: 0, low: 3)
        _ = try await store.upsert([
            IngestDocument(id: superseded, revision: 1, timestamp: 1, vector: [1], text: "old"),
            IngestDocument(id: tombstoned, revision: 1, timestamp: 2, vector: [2]),
        ])
        _ = try await store.upsert([
            IngestDocument(id: superseded, revision: 2, timestamp: 4, vector: [4], text: "new")
        ])
        let deletion = try await store.delete([tombstoned])

        let result = try await store.get([superseded, missing, tombstoned])
        XCTAssertEqual(result.documents.count, 3)
        XCTAssertEqual(result.documents[0]?.revision, 2)
        XCTAssertEqual(result.documents[0]?.vector ?? [], [4])
        XCTAssertEqual(result.documents[0]?.text, "new")
        XCTAssertNil(result.documents[1])
        XCTAssertNil(result.documents[2])
        XCTAssertEqual(result.missingCount, 2)
        XCTAssertEqual(result.generation, deletion.generation)
        try await store.close()
    }

    func testScanOrdersAcrossSealedAndActiveSegments() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "scan",
            spec: NamespaceSpec(
                attributes: [],
                vectorSpace: VectorSpace(dimensions: 1, normalization: .none)
            )
        )
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 3), revision: 1, timestamp: 20, vector: [3]),
            IngestDocument(id: DocumentID(high: 0, low: 2), revision: 1, timestamp: 10, vector: [2]),
        ])
        _ = try await store.seal()
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 1), revision: 1, timestamp: 20, vector: [1]),
            IngestDocument(id: DocumentID(high: 0, low: 4), revision: 1, timestamp: 30, vector: [4]),
        ])

        var cursor: ScanCursor?
        var storageIDs: [UInt64] = []
        repeat {
            let page = try await store.scan(after: cursor, limit: 2, fields: [])
            storageIDs.append(contentsOf: page.documents.map { $0.id.low })
            cursor = page.next
        } while cursor != nil
        XCTAssertEqual(storageIDs, [3, 2, 1, 4])

        let ascending = try await store.scan(limit: 10, order: .timestampAscending, fields: [])
        XCTAssertEqual(ascending.documents.map { $0.id.low }, [2, 1, 3, 4])
        let descending = try await store.scan(limit: 10, order: .timestampDescending, fields: [])
        XCTAssertEqual(descending.documents.map { $0.id.low }, [4, 1, 3, 2])
        try await store.close()
    }

    func testScanStaleCursorThrowsWithoutRestarting() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "stale",
            spec: NamespaceSpec(
                attributes: [],
                vectorSpace: VectorSpace(dimensions: 1, normalization: .none)
            )
        )
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 1), revision: 1, timestamp: 1, vector: [1]),
            IngestDocument(id: DocumentID(high: 0, low: 2), revision: 1, timestamp: 2, vector: [2]),
        ])
        let first = try await store.scan(limit: 1)
        let cursor = try XCTUnwrap(first.next)
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 3), revision: 1, timestamp: 3, vector: [3])
        ])
        _ = try await store.seal()

        do {
            _ = try await store.scan(after: cursor, limit: 1)
            XCTFail("stale scan restarted")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .scanStale)
        }
        try await store.close()
    }

    func testCountAndFilteredSearchMatchScannedAndExactResults() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "filtered",
            spec: NamespaceSpec(
                attributes: [
                    AttributeDefinition(id: 1, name: "group", type: .u64, nullable: false),
                    AttributeDefinition(id: 2, name: "label", type: .rawString, nullable: true),
                ],
                vectorSpace: VectorSpace(dimensions: 2, normalization: .unitL2)
            )
        )
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 1), revision: 1, timestamp: 10, vector: [1, 0], attributes: [1: .u64(7), 2: .string("alpha")]),
            IngestDocument(id: DocumentID(high: 0, low: 2), revision: 1, timestamp: 20, vector: [0.8, 0.6], attributes: [1: .u64(8), 2: .string("alpha")]),
            IngestDocument(id: DocumentID(high: 0, low: 3), revision: 1, timestamp: 30, vector: [0, 1], attributes: [1: .u64(7), 2: .null]),
            IngestDocument(id: DocumentID(high: 0, low: 4), revision: 1, timestamp: 40, vector: [-1, 0], attributes: [1: .u64(8), 2: .string("bravo")]),
        ])
        let filter = Filter.and([
            .eq(1, .u64(7)),
            .or([.eq(2, .string("alpha")), .not(.exists(2))]),
        ])

        var scanned: [StoredDocument] = []
        for try await document in store.documents(fields: [], filter: filter) {
            scanned.append(document)
        }
        let count = try await store.count(filter: filter)
        XCTAssertEqual(count.count, UInt64(scanned.count))
        XCTAssertEqual(Set(scanned.map { $0.id.low }), Set([1, 3]))

        let exact = try await store.search(
            vector: [1, 0],
            options: SearchOptions(k: 4, threadBudget: 1, tier: .exact)
        )
        let expected = exact.hits.filter { hit in
            scanned.contains { $0.id == hit.documentID }
        }
        let filtered = try await store.search(
            vector: [1, 0],
            filter: filter,
            options: SearchOptions(k: 4, threadBudget: 1, tier: .exact)
        )
        XCTAssertEqual(filtered.hits.map(\.documentID), expected.map(\.documentID))
        XCTAssertEqual(filtered.hits.map(\.score), expected.map(\.score))
        try await store.close()
    }

    func testRecordOnlyNamespaceReadsRecordsAndRejectsVectorSearch() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "records",
            spec: NamespaceSpec(
                attributes: [
                    AttributeDefinition(id: 1, name: "kind", type: .rawString, nullable: false)
                ],
                vectorSpace: nil
            )
        )
        let id = DocumentID(high: 0, low: 7)
        _ = try await store.upsert([
            IngestDocument(
                id: id,
                revision: 1,
                timestamp: 9,
                vector: [],
                text: "record only",
                attributes: [1: .string("note")]
            )
        ])
        let fetched = try await store.get([id])
        XCTAssertNil(fetched.documents[0]?.vector)
        XCTAssertEqual(fetched.documents[0]?.text, "record only")
        let scanned = try await store.scan()
        XCTAssertEqual(scanned.documents.map(\.id), [id])

        do {
            _ = try await store.search(
                vector: [1],
                options: SearchOptions(k: 1, tier: .exact)
            )
            XCTFail("record-only vector search succeeded")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .noVectorSpace)
        }
        try await store.close()
    }

    func testLastErrorMessageReturnsDeliberateFailureDetail() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "errors",
            spec: NamespaceSpec(attributes: [], vectorSpace: nil)
        )
        do {
            _ = try await store.count(filter: .exists(99))
            XCTFail("unknown attribute count succeeded")
        } catch let error as ZeppelinError {
            XCTAssertEqual(error, .invalidArgument)
        }
        let captured = await store.lastErrorMessage()
        let message = try XCTUnwrap(captured)
        XCTAssertTrue(message.contains("unknown column 99"), message)
        try await store.close()
    }

    func testABIUUIDAndUnitVectorCosineUtilities() throws {
        XCTAssertEqual(ZeppelinStore.abiVersion, 1)
        let uuid = try XCTUnwrap(UUID(uuidString: "00112233-4455-6677-8899-AABBCCDDEEFF"))
        let id = DocumentID(uuid: uuid)
        XCTAssertEqual(id.high, 0x0011_2233_4455_6677)
        XCTAssertEqual(id.low, 0x8899_AABB_CCDD_EEFF)
        XCTAssertEqual(id.uuid, uuid)

        let hit = SearchHit(
            sourceKind: 0,
            segmentID: [],
            localRow: 0,
            documentID: id,
            revision: 1,
            score: -0.5
        )
        XCTAssertEqual(hit.cosineSimilarity, 0.75, accuracy: 0.000_001)
        XCTAssertEqual(SearchOptions.scoreFloor(cosine: 0.75), -0.5, accuracy: 0.000_001)
    }

    func testExportImportJSONLinesRoundTripsEveryField() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let exportURL = root.appendingPathComponent("records.jsonl")
        let spec = NamespaceSpec(
            attributes: [
                AttributeDefinition(id: 1, name: "rank", type: .u64, nullable: false),
                AttributeDefinition(id: 2, name: "label", type: .rawString, nullable: false),
            ],
            vectorSpace: VectorSpace(dimensions: 2, normalization: .none)
        )
        let source = try await ZeppelinStore.openNamespace(
            root: root,
            name: "source",
            spec: spec
        )
        let expected = IngestDocument(
            id: DocumentID(high: 0x1234, low: 0x5678),
            revision: 9,
            timestamp: -42,
            vector: [1.25, -2.5],
            text: "zeppelin \u{1F680}",
            metadata: Data([0, 1, 2, 255]),
            attributes: [1: .u64(7), 2: .string("blue")]
        )
        _ = try await source.upsert([expected])
        try await source.exportJSONLines(to: exportURL, fields: .all)
        try await source.close()

        let destination = try await ZeppelinStore.openNamespace(
            root: root,
            name: "destination",
            spec: spec
        )
        try await destination.importJSONLines(from: exportURL)
        let result = try await destination.get([expected.id])
        let actual = try XCTUnwrap(result.documents[0])
        XCTAssertEqual(actual.id, expected.id)
        XCTAssertEqual(actual.revision, expected.revision)
        XCTAssertEqual(actual.timestamp, expected.timestamp)
        XCTAssertEqual(actual.vector ?? [], expected.vector)
        XCTAssertEqual(actual.text, expected.text)
        XCTAssertEqual(actual.metadata, expected.metadata)
        XCTAssertEqual(actual.attributes, expected.attributes)
        try await destination.close()
    }

    func testMaintenancePolicySealsAtHostScheduledRowThreshold() async throws {
        let root = try namespaceRoot(#function)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = try await ZeppelinStore.openNamespace(
            root: root,
            name: "maintenance",
            spec: NamespaceSpec(
                attributes: [],
                vectorSpace: VectorSpace(dimensions: 1, normalization: .none)
            )
        )
        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 1), revision: 1, timestamp: 1, vector: [1]),
            IngestDocument(id: DocumentID(high: 0, low: 2), revision: 1, timestamp: 2, vector: [2]),
        ])
        let policy = MaintenancePolicy(
            sealAtActiveRowCount: 2,
            wallTimeNanoseconds: 0,
            byteBudget: 0
        )
        _ = try await policy.run(on: store)
        let stats = try await store.stats()
        XCTAssertEqual(stats.activeRowCount, 0)
        XCTAssertGreaterThan(stats.segmentBytes, 0)

        _ = try await store.upsert([
            IngestDocument(id: DocumentID(high: 0, low: 3), revision: 1, timestamp: 3, vector: [3])
        ])
        let idlePolicy = MaintenancePolicy(
            sealAtActiveRowCount: 100,
            idleInterval: .seconds(5),
            wallTimeNanoseconds: 0,
            byteBudget: 0
        )
        _ = try await idlePolicy.run(on: store, idleFor: .seconds(6))
        let idleStats = try await store.stats()
        XCTAssertEqual(idleStats.activeRowCount, 0)
        try await store.close()
    }

    func testLocalXCFrameworkLoadsWhenRequested() throws {
        guard ProcessInfo.processInfo.environment["ZE_USE_LOCAL_XCFRAMEWORK"] == "1" else {
            throw XCTSkip("exercised by swift-release.yml after the XCFramework is built")
        }
        XCTAssertEqual(ZeppelinStore.abiVersion, 1)
    }

    private func namespaceRoot(_ name: String) throws -> URL {
        let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".build/test-namespaces", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root.appendingPathComponent("\(name)-\(UUID().uuidString)", isDirectory: true)
    }
}
