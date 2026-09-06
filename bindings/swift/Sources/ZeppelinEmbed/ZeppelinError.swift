// Generated from ffi_contract::ERROR_CODE_GOLDEN. Do not edit by hand.

public enum ZeppelinError: Int32, Error, Sendable, CaseIterable {
    case ok = 0
    case invalidArgument = 1
    case invalidHandle = 2
    case closed = 3
    case closing = 4
    case poisoned = 5
    case panic = 6
    case busy = 7
    case storeBusy = 8
    case io = 9
    case corrupt = 10
    case unsupported = 11
    case cancelled = 12
    case timeout = 13
    case outOfMemory = 14
    case budgetExceeded = 15
    case emptyBatch = 16
    case staleRevision = 17
    case dimensionMismatch = 18
    case notFound = 19
    case synchronization = 20
    case accessMode = 21
    case internalError = 22
    case epochMismatch = 23
    case epochUndeclared = 24
    case epochUnstamped = 25
    case epochIncomplete = 26
    case epochPublished = 27
    case unsealedWrites = 28
    case bundle = 29
    case model = 30
    case pipeline = 31
    case scanStale = 32
    case schemaMismatch = 33
    case noVectorSpace = 34
}
