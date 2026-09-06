import CZeppelinEmbed
import Foundation

public final class ZeppelinCancellationToken: @unchecked Sendable {
    private let lock = NSLock()
    private var token: UInt64?

    private init(token: UInt64) {
        self.token = token
    }

    deinit {
        let value = takeToken()
        if let value {
            _ = ze_cancel_token_free(value)
        }
    }

    public static func create() async throws -> ZeppelinCancellationToken {
        try await Task.detached {
            var value: UInt64 = 0
            try checkZeppelin(ze_cancel_token_create(&value))
            return ZeppelinCancellationToken(token: value)
        }.value
    }

    public func cancel() async throws {
        let value = try currentToken()
        try await Task.detached {
            try checkZeppelin(ze_cancel_token_cancel(value))
        }.value
    }

    public func free() async throws {
        guard let value = takeToken() else {
            return
        }
        try await Task.detached {
            try checkZeppelin(ze_cancel_token_free(value))
        }.value
    }

    @_spi(Testing)
    public func cancelImmediatelyForTesting() throws {
        try checkZeppelin(ze_cancel_token_cancel(try currentToken()))
    }

    internal func rawValue() throws -> UInt64 {
        try currentToken()
    }

    private func currentToken() throws -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        guard let token else {
            throw ZeppelinError.closed
        }
        return token
    }

    private func takeToken() -> UInt64? {
        lock.lock()
        defer { lock.unlock() }
        let value = token
        token = nil
        return value
    }
}
