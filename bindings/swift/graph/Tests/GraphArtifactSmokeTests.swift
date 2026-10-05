import CZeppelinEmbedGraph
import XCTest

final class GraphArtifactSmokeTests: XCTestCase {
  func testGraphArtifactImportsCoreAndRunsCypher() {
    XCTAssertGreaterThan(ze_abi_version(), 0)
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: directory) }
    let path = Array(directory.path.utf8)
    var handle = ZeGraphHandle()
    path.withUnsafeBufferPointer { bytes in
      var request = ZeGraphOpenRequest()
      request.abi_size = UInt32(MemoryLayout<ZeGraphOpenRequest>.size)
      request.path = ZeGraphBytes(data: bytes.baseAddress, count: bytes.count)
      request.max_resident_bytes = 256 << 20
      request.reader_drain_timeout_ms = 250
      XCTAssertEqual(ze_graph_open(&request, &handle), Int32(ZE_OK.rawValue))
    }
    defer { XCTAssertEqual(ze_graph_close(handle), Int32(ZE_OK.rawValue)) }
    for (text, generation, rows) in [
      ("CREATE (:Doc {title: 'alpha'})", UInt64(1), 0),
      ("MATCH (n:Doc) RETURN n.title AS title", UInt64(1), 1),
    ] {
      let query = Array(text.utf8)
      query.withUnsafeBufferPointer { bytes in
        var request = ZeGraphCypherRequest()
        request.abi_size = UInt32(MemoryLayout<ZeGraphCypherRequest>.size)
        request.query = ZeGraphBytes(data: bytes.baseAddress, count: bytes.count)
        var response = ZeGraphResponse()
        response.abi_size = UInt32(MemoryLayout<ZeGraphResponse>.size)
        response.pool.abi_size = UInt32(MemoryLayout<ZeGraphValuePool>.size)
        XCTAssertEqual(ze_graph_cypher(handle, &request, &response), Int32(ZE_OK.rawValue))
        XCTAssertEqual(response.row_count, rows)
        XCTAssertEqual(
          rows == 0 ? response.changed_generation : response.admitted_generation, generation)
        XCTAssertEqual(ze_graph_response_free(&response), Int32(ZE_OK.rawValue))
      }
    }
  }
}
