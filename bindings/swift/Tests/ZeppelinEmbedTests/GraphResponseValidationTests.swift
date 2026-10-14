#if ZE_GRAPH
import CZeppelinEmbed
import XCTest
@testable import ZeppelinEmbed

final class GraphResponseValidationTests: XCTestCase {
  func testZeroColumnsRejectBogusRowCountsAndFreeResponse() throws {
    for count in [-1, Int.min, 65537, Int.max] {
      var frees = 0
      XCTAssertThrowsError(try graphResponse(
        call: { response in
          response.disposition = 2
          response.has_changed_generation = 1
          response.changed_generation = 19
          response.row_count = count
          return 0
        }, free: { _ in frees += 1; return 0 }
      )) { error in
        guard let error = error as? GraphError,
          case .invalidResponse("row shape") = error.reason
        else { return XCTFail("expected invalid row shape") }
        XCTAssertEqual(error.metadata?.changedGeneration, 19)
      }
      XCTAssertEqual(frees, 1)
    }
  }

  func testResourceFailureIncludesNativeMessage() async throws {
    let store = makeGraphTestStore(handle: .max, calls: GraphNativeCalls(close: { _ in 0 }))
    do {
      _ = try await store.graphResources()
      XCTFail("invalid handle accepted")
    } catch let error as GraphError {
      XCTAssertEqual(error.code, .invalidHandle)
      XCTAssertEqual(error.nativeMessage, "native diagnostic unavailable (status 2)")
    }
    try await store.close()
  }
}
#endif
