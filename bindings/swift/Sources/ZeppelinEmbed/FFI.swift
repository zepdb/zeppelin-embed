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
