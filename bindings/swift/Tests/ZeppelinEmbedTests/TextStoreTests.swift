#if ZE_TEXT
import Foundation
import CryptoKit
import XCTest
@testable import ZeppelinEmbed

final class TextStoreTests: XCTestCase {
    private var root: URL!
    private var repository: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
    }
    private var bundle: URL {
        repository.appendingPathComponent("crates/zeppelin-embed-text/tests/fixtures/ze115/gte.zem")
    }
    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("ze509-\(UUID())")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }
    override func tearDownWithError() throws {
        try FileManager.default.removeItem(at: root)
    }

    func testTextOpenHealthAndClose() async throws {
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: bundle)
        let health = try await store.health()
        XCTAssertEqual(health.pendingDocuments, 0)
        XCTAssertEqual(health.unresolvedFaultCount, 0)
        try await store.close()
        try await store.close()
        do {
            _ = try await store.health()
            XCTFail("closed store must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .closed) }
    }
    func testTextDenseLexicalHybridReingestDeleteAndMaintain() async throws {
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: bundle)
        let id = DocumentID(high: 0, low: 1)
        let report = try await store.ingest([TextDocument(id: id, revision: 1, text: "bronze zeppelin")])
        XCTAssertEqual(report.documents, 1)
        XCTAssertEqual(report.chunks, 1)
        XCTAssertTrue(report.allThreadsJoined)
        for legs in [TextLegs.dense, .lexical, .hybrid] {
            let result = try await store.query("bronze", legs: legs, k: 10, tier: .exact)
            XCTAssertEqual(result.hits.count, 1)
            XCTAssertEqual(result.hits.first?.id, id)
            XCTAssertEqual(result.hits.first?.text, "bronze zeppelin")
            XCTAssertTrue(result.hits.allSatisfy { $0.score.isFinite })
        }
        var chunking = TextIngestOptions()
        chunking.chunkPolicy = .tokens(max: 1, overlap: 0)
        let expanded = try await store.ingest([
            TextDocument(id: id, revision: 2, text: "bronze zeppelin")], options: chunking)
        XCTAssertEqual(expanded.chunks, 2)
        _ = try await store.ingest([TextDocument(id: id, revision: 3, text: "bronze")], options: chunking)
        let shorter = try await store.query("bronze", legs: .dense, k: 10)
        XCTAssertEqual(shorter.hits.count, 1)
        XCTAssertEqual(shorter.hits.first?.revision, 3)
        XCTAssertEqual(shorter.hits.first?.text, "bronze")
        let staleTail = try await store.query("zeppelin", legs: .lexical, k: 10)
        XCTAssertTrue(staleTail.hits.isEmpty)
        let maintenance = try await store.maintain(wallTimeNanoseconds: 1_000_000_000, byteBudget: 64 * 1024 * 1024)
        XCTAssertEqual(maintenance.status, .complete)
        let deleted = try await store.delete([id])
        XCTAssertGreaterThan(deleted.generation, report.generation)
        for legs in [TextLegs.lexical, .hybrid] {
            let result = try await store.query("bronze", legs: legs, k: 10)
            XCTAssertTrue(result.hits.isEmpty, "delete-emptied store: \(legs)")
        }
        try await store.close()
    }

    func testTextEmptyStoreLexicalAndHybridReturnZeroHits() async throws {
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: bundle)
        for legs in [TextLegs.lexical, .hybrid] {
            let result = try await store.query("hello", legs: legs, k: 10)
            XCTAssertTrue(result.hits.isEmpty, "new empty store: \(legs)")
        }
        try await store.close()
    }

    func testTextTaskCancellationAndDeadline() async throws {
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: bundle)
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await store.query("hello", legs: .dense, k: 1)
        }
        do {
            _ = try await cancelled.value
            XCTFail("cancelled Task must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .cancelled) }
        do {
            _ = try await store.query("hello", legs: .dense, k: 1, deadlineNanoseconds: 1)
            XCTFail("expired native deadline must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .timeout) }
        let document = TextDocument(id: DocumentID(high: 0, low: 1), revision: 1, text: "bronze")
        let cancelledIngest = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await store.ingest([document])
        }
        do {
            _ = try await cancelledIngest.value
            XCTFail("cancelled ingest must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .cancelled) }
        do {
            _ = try await store.ingest([document], deadlineNanoseconds: 1)
            XCTFail("expired ingest deadline must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .timeout) }
        let empty = try await store.query("bronze", legs: .dense, k: 1)
        XCTAssertTrue(empty.hits.isEmpty)
        try await store.close()
    }

    func testTextOpenModelOfflineMiss() async throws {
        let model = TextModel(repo: "fixture/tiny", revision: String(repeating: "a", count: 40),
            manifestSHA256: String(repeating: "0", count: 64))
        do {
            _ = try await TextStore.open(at: root.appendingPathComponent("store"), model: model,
                cacheDirectory: root.appendingPathComponent("cache"), offline: true)
            XCTFail("offline cache miss must fail")
        } catch { XCTAssertEqual(error as? ZeppelinError, .notFound) }
    }

    func testTextOpenModelHTTPProgressAndOfflineCache() async throws {
        let serverRoot = root.appendingPathComponent("http")
        let revision = String(repeating: "a", count: 40)
        let directory = serverRoot.appendingPathComponent("fixture/tiny/resolve/" + revision)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let body = try Data(contentsOf: bundle)
        try body.write(to: directory.appendingPathComponent("tiny.zem"))
        let manifest = Data("""
        {"format":1,"name":"tiny","bundle":"tiny.zem","query_coreml":null,"files":[{"path":"tiny.zem","size":\(body.count),"sha256":"\(digest(body))"}]}
        """.utf8)
        try manifest.write(to: directory.appendingPathComponent("zeppelin-model.json"))
        let model = TextModel(repo: "fixture/tiny", revision: revision, manifestSHA256: digest(manifest))
        let server = Process()
        let output = Pipe()
        server.executableURL = URL(fileURLWithPath: "/usr/bin/python3")
        server.arguments = ["-u", "-c", """
        import http.server, os, sys
        os.chdir(sys.argv[1])
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler)
        print(server.server_port, flush=True)
        server.serve_forever()
        """, serverRoot.path]
        server.standardOutput = output
        try server.run()
        defer { server.terminate(); server.waitUntilExit() }
        var portBytes = Data()
        while true {
            let byte = output.fileHandleForReading.readData(ofLength: 1)
            guard !byte.isEmpty else { throw ZeppelinError.io }
            if byte == Data([10]) { break }
            portBytes.append(byte)
        }
        let port = try XCTUnwrap(Int(String(decoding: portBytes, as: UTF8.self)))
        let oldEndpoint = ProcessInfo.processInfo.environment["ZE_MODEL_ENDPOINT"]
        let oldHTTP = ProcessInfo.processInfo.environment["ZE_MODEL_ALLOW_HTTP"]
        setenv("ZE_MODEL_ENDPOINT", "http://127.0.0.1:\(port)", 1)
        setenv("ZE_MODEL_ALLOW_HTTP", "1", 1)
        defer {
            restoreEnvironment("ZE_MODEL_ENDPOINT", oldEndpoint)
            restoreEnvironment("ZE_MODEL_ALLOW_HTTP", oldHTTP)
        }
        let progress = ProgressLog()
        let cache = root.appendingPathComponent("cache")
        let store = try await TextStore.open(at: root.appendingPathComponent("download"), model: model,
            cacheDirectory: cache, progress: { progress.append($0) })
        XCTAssertEqual(progress.values.last?.completedBytes, UInt64(body.count))
        XCTAssertEqual(progress.values.last?.totalBytes, UInt64(body.count))
        XCTAssertEqual(progress.values.first?.completedBytes, 0)
        XCTAssertTrue(zip(progress.values, progress.values.dropFirst()).allSatisfy {
            $0.completedBytes <= $1.completedBytes
        })
        try await store.close()
        let offline = try await TextStore.open(at: root.appendingPathComponent("offline"), model: model,
            cacheDirectory: cache, offline: true, progress: { _ in XCTFail("cache hit must not download") })
        try await offline.close()
    }

    func testTextObservedComputeUnitsAbsentWithCoreMLOff() async throws {
        let old = ProcessInfo.processInfo.environment["ZE_QUERY_COREML"]
        setenv("ZE_QUERY_COREML", "off", 1)
        defer { restoreEnvironment("ZE_QUERY_COREML", old) }
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: bundle)
        let counts = try await store.observedComputeUnits()
        XCTAssertNil(counts)
        try await store.close()
    }

    func testTextObservedComputeUnitsTinyCoreML() async throws {
        guard #available(macOS 14.4, *) else {
            throw XCTSkip("CoreML compute plans require macOS 14.4")
        }
        let compiler = Process()
        compiler.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        compiler.arguments = ["coremlcompiler", "compile", repository.appendingPathComponent(
            "crates/zeppelin-embed-text/tests/fixtures/ze115/tiny.mlmodel").path, root.path]
        try compiler.run()
        compiler.waitUntilExit()
        XCTAssertEqual(compiler.terminationStatus, 0)
        let old = ProcessInfo.processInfo.environment["ZE_QUERY_COREML"]
        setenv("ZE_QUERY_COREML", root.appendingPathComponent("tiny.mlmodelc").path, 1)
        defer { restoreEnvironment("ZE_QUERY_COREML", old) }
        let symmetric = repository.appendingPathComponent("crates/zeppelin-embed-ffi/tests/fixtures/compute_units.zem")
        let store = try await TextStore.open(at: root.appendingPathComponent("store"), bundle: symmetric)
        let counts = try await store.observedComputeUnits()
        let observed = try XCTUnwrap(counts)
        XCTAssertEqual(observed.cpu + observed.gpu + observed.neuralEngine + observed.other, 1)
        try await store.close()
    }

    private func digest(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
    private func restoreEnvironment(_ name: String, _ value: String?) {
        if let value { setenv(name, value, 1) } else { unsetenv(name) }
    }

}
private final class ProgressLog: @unchecked Sendable {
    private let lock = NSLock()
    private var reports: [TextDownloadProgress] = []
    func append(_ report: TextDownloadProgress) {
        lock.lock(); defer { lock.unlock() }
        reports.append(report)
    }
    var values: [TextDownloadProgress] {
        lock.lock(); defer { lock.unlock() }
        return reports
    }
}
#endif
