import Foundation
import ZeppelinEmbed

@main
struct MacDemo {
    static func main() async throws {
        let applicationSupport = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let path = applicationSupport.appendingPathComponent("ZeppelinEmbed-MacDemo")
        try await ZeppelinStore.withStore(at: path) { store in
            let stats = try await store.stats()
            print("ZeppelinEmbed active rows: \(stats.activeRowCount)")
        }
    }
}
