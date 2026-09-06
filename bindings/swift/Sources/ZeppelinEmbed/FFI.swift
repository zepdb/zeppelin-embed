import CZeppelinEmbed

@inline(__always)
internal func checkZeppelin(_ code: Int32) throws {
    guard code == 0 else {
        throw ZeppelinError(rawValue: code) ?? .internalError
    }
}

@inline(__always)
internal func abiSize<T>(_: T.Type) -> UInt32 {
    UInt32(MemoryLayout<T>.size)
}

@available(macOS 14.0, iOS 17.0, *)
extension ZeppelinStore {
    public static var abiVersion: UInt32 {
        ze_abi_version()
    }

    public func lastErrorMessage() async -> String? {
        let current = lastErrorHandle()
        do {
            return try await Self.runBlocking {
                var required = 0
                guard ze_last_error_message(current, nil, 0, &required) == 0,
                    required > 0
                else {
                    return nil
                }
                var bytes = [CChar](repeating: 0, count: required + 1)
                let code = bytes.withUnsafeMutableBufferPointer { buffer in
                    ze_last_error_message(current, buffer.baseAddress, buffer.count, &required)
                }
                guard code == 0 else {
                    return nil
                }
                return String(decoding: bytes.prefix(required).map { UInt8(bitPattern: $0) }, as: UTF8.self)
            }
        } catch {
            return nil
        }
    }
}
