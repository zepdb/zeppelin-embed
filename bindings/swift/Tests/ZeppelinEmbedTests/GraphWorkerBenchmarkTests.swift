#if ZE_GRAPH
import CZeppelinEmbed
import Foundation
import XCTest
@testable import ZeppelinEmbed

final class GraphWorkerBenchmarkTests: XCTestCase {
  func testManySmallBlockingCalls() async throws {
    guard ProcessInfo.processInfo.environment["ZE324_BENCHMARK"] == "1" else {
      throw XCTSkip("opt-in measurement; no wall-clock gate")
    }
    let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? FileManager.default.removeItem(at: path) }
    let store = try await openGraphTestStore(at: path, mode: .create)
    let handle = try await store.openHandle()
    let samples = 1000
    for _ in 0..<100 { _ = try await store.graphResources() }
    for repetition in 0..<7 {
      let directStart = DispatchTime.now().uptimeNanoseconds
      for _ in 0..<samples {
        var resources = ZeGraphResources()
        resources.abi_size = graphSize(ZeGraphResources.self)
        XCTAssertEqual(ze_store_graph_resources(handle, &resources), 0)
      }
      let direct = DispatchTime.now().uptimeNanoseconds - directStart
      let blockingStart = DispatchTime.now().uptimeNanoseconds
      for _ in 0..<samples { _ = try await store.graphResources() }
      let blocking = DispatchTime.now().uptimeNanoseconds - blockingStart
      print("ZE324 repetition=\(repetition) calls=\(samples) direct_ns=\(direct) blocking_ns=\(blocking)")
    }
    try await store.close()
  }
}
#endif
