import Foundation

public struct DocumentID: Hashable, Sendable {
    public var high: UInt64
    public var low: UInt64

    public init(high: UInt64, low: UInt64) {
        self.high = high
        self.low = low
    }

    public init(uuid: UUID) {
        var bytes = uuid.uuid
        let values = withUnsafeBytes(of: &bytes) { Array($0.prefix(16)) }
        high = values[0..<8].reduce(0) { ($0 << 8) | UInt64($1) }
        low = values[8..<16].reduce(0) { ($0 << 8) | UInt64($1) }
    }

    public var uuid: UUID {
        get {
            UUID(uuid: (
                UInt8(truncatingIfNeeded: high >> 56),
                UInt8(truncatingIfNeeded: high >> 48),
                UInt8(truncatingIfNeeded: high >> 40),
                UInt8(truncatingIfNeeded: high >> 32),
                UInt8(truncatingIfNeeded: high >> 24),
                UInt8(truncatingIfNeeded: high >> 16),
                UInt8(truncatingIfNeeded: high >> 8),
                UInt8(truncatingIfNeeded: high),
                UInt8(truncatingIfNeeded: low >> 56),
                UInt8(truncatingIfNeeded: low >> 48),
                UInt8(truncatingIfNeeded: low >> 40),
                UInt8(truncatingIfNeeded: low >> 32),
                UInt8(truncatingIfNeeded: low >> 24),
                UInt8(truncatingIfNeeded: low >> 16),
                UInt8(truncatingIfNeeded: low >> 8),
                UInt8(truncatingIfNeeded: low)
            ))
        }
        set {
            self = DocumentID(uuid: newValue)
        }
    }
}

public enum AccessMode: Int32, Sendable {
    case readWrite = 0
    case readOnly = 1
}

public enum DurabilityMode: Int32, Sendable {
    case derived = 0
    case durable = 1
    case attached = 2
}

public enum CommitTier: Int32, Sendable {
    case none = 0
    case ordered = 1
    case durable = 2
}

public struct OpenOptions: Sendable {
    public var accessMode: AccessMode
    public var durabilityMode: DurabilityMode
    public var commitTier: CommitTier
    public var readerDrainTimeoutMilliseconds: UInt64
    public var maxResidentBytes: UInt64
    public var maxTemporaryBytes: UInt64
    public var excludeFromBackup: Bool

    public init(
        accessMode: AccessMode = .readWrite,
        durabilityMode: DurabilityMode = .derived,
        commitTier: CommitTier = .none,
        readerDrainTimeoutMilliseconds: UInt64 = 5_000,
        maxResidentBytes: UInt64 = 512 * 1_024 * 1_024,
        maxTemporaryBytes: UInt64 = 512 * 1_024 * 1_024,
        excludeFromBackup: Bool = true
    ) {
        self.accessMode = accessMode
        self.durabilityMode = durabilityMode
        self.commitTier = commitTier
        self.readerDrainTimeoutMilliseconds = readerDrainTimeoutMilliseconds
        self.maxResidentBytes = maxResidentBytes
        self.maxTemporaryBytes = maxTemporaryBytes
        self.excludeFromBackup = excludeFromBackup
    }
}

public enum VectorNormalization: Int32, Sendable {
    case none = 0
    case unitL2 = 1
}

public enum EmbeddingRuntime: Int32, Sendable {
    case coreML = 1
    case mlx = 2
    case cpuReference = 3
}

public enum ComputeUnits: Int32, Sendable {
    case cpu = 1
    case cpuAndGPU = 2
    case cpuAndNeuralEngine = 3
    case all = 4
}

public struct EmbeddingTower: Sendable {
    public var modelID: String
    public var modelVersion: String
    public var weightsDigest: Data
    public var dimensions: UInt32
    public var normalization: VectorNormalization
    public var promptPrefix: String
    public var maxTokens: UInt32
    public var runtime: EmbeddingRuntime
    public var computeUnits: ComputeUnits
    public var operatingSystemBuild: String?

    public init(
        modelID: String,
        modelVersion: String,
        weightsDigest: Data,
        dimensions: UInt32,
        normalization: VectorNormalization = .none,
        promptPrefix: String = "",
        maxTokens: UInt32,
        runtime: EmbeddingRuntime,
        computeUnits: ComputeUnits,
        operatingSystemBuild: String? = nil
    ) {
        self.modelID = modelID
        self.modelVersion = modelVersion
        self.weightsDigest = weightsDigest
        self.dimensions = dimensions
        self.normalization = normalization
        self.promptPrefix = promptPrefix
        self.maxTokens = maxTokens
        self.runtime = runtime
        self.computeUnits = computeUnits
        self.operatingSystemBuild = operatingSystemBuild
    }
}

public struct EmbeddingEpoch: Sendable {
    public var document: EmbeddingTower
    public var query: EmbeddingTower
    public var alignmentDigest: Data

    public init(document: EmbeddingTower, query: EmbeddingTower, alignmentDigest: Data = Data()) {
        self.document = document
        self.query = query
        self.alignmentDigest = alignmentDigest
    }
}

public struct Epoch: Sendable {
    public var embedding: EmbeddingEpoch

    public init(embedding: EmbeddingEpoch) {
        self.embedding = embedding
    }
}

public struct EpochIdentity: Equatable, Sendable {
    public var embeddingEpoch: UInt64
    public var tokenizerEpoch: UInt64
}

public struct EpochAliasReport: Equatable, Sendable {
    public var generation: UInt64
    public var previous: EpochIdentity
    public var published: EpochIdentity
    public var manifestCommitted: Bool
}

public struct EpochDropReport: Equatable, Sendable {
    public var generation: UInt64
    public var segmentsDropped: UInt64
    public var bytesReclaimed: UInt64
}

public enum StoreState: Int32, Sendable {
    case open = 0
    case closing = 1
    case closed = 2
}

public struct StoreStats: Equatable, Sendable {
    public var residentOwnedBytes: UInt64
    public var mappedBytes: UInt64
    public var mappedResidentBytes: UInt64
    public var segmentBytes: UInt64
    public var activeSegmentBytes: UInt64
    public var activeRowCount: UInt64
    public var tombstoneCount: UInt64
    public var tombstoneBytes: UInt64
    public var walBytes: UInt64
    public var cacheBytes: UInt64
    public var temporaryBytes: UInt64
    public var queryPoolBytes: UInt64
    public var openFiles: UInt64
    public var activeQueries: UInt64
    public var activeSnapshotLeases: UInt64
    public var physicalFootprint: UInt64?
}

public struct IngestDocument: Sendable {
    public var id: DocumentID
    public var revision: UInt64
    public var timestamp: Int64
    public var vector: [Float]
    public var text: String?
    public var metadata: Data
    public var attributes: [UInt32: AttributeValue]

    public init(
        id: DocumentID,
        revision: UInt64,
        timestamp: Int64,
        vector: [Float],
        text: String? = nil,
        metadata: Data = Data(),
        attributes: [UInt32: AttributeValue] = [:]
    ) {
        self.id = id
        self.revision = revision
        self.timestamp = timestamp
        self.vector = vector
        self.text = text
        self.metadata = metadata
        self.attributes = attributes
    }
}

public struct MutationReport: Equatable, Sendable {
    public var sequence: UInt64
    public var generation: UInt64
}

public enum SearchTier: Int32, Sendable {
    case automatic = 0
    case exact = 1
    case scan = 2
    case graph = 3
}

public enum GraphProfile: Int32, Sendable {
    case sift = 0
    case angular = 1
}

public struct SearchOptions: Sendable {
    public var k: Int
    public var threadBudget: Int
    public var tier: SearchTier?
    public var graphProfile: GraphProfile
    public var graphEF: Int
    public var graphSeed: UInt64
    public var cancellationToken: ZeppelinCancellationToken?
    public var deadlineNanoseconds: UInt64

    public init() {
        self.init(k: 10)
    }

    public init(
        k: Int,
        threadBudget: Int = 0,
        tier: SearchTier? = nil,
        graphProfile: GraphProfile = .sift,
        graphEF: Int = 0,
        graphSeed: UInt64 = 0,
        cancellationToken: ZeppelinCancellationToken? = nil,
        deadlineNanoseconds: UInt64 = 0
    ) {
        self.k = k
        self.threadBudget = threadBudget
        self.tier = tier
        self.graphProfile = graphProfile
        self.graphEF = graphEF
        self.graphSeed = graphSeed
        self.cancellationToken = cancellationToken
        self.deadlineNanoseconds = deadlineNanoseconds
    }

    /// Converts a cosine threshold for unit-normalized vectors to Zeppelin's
    /// larger-is-better negative-squared-L2 score.
    public static func scoreFloor(cosine: Float) -> Float {
        -2 * (1 - cosine)
    }
}

public struct SearchHit: Equatable, Sendable {
    public var sourceKind: UInt32
    public var segmentID: [UInt8]
    public var localRow: UInt32
    public var documentID: DocumentID?
    public var revision: UInt64?
    public var score: Float

    /// Cosine similarity derived as `1 - d² / 2`. Valid only when both
    /// stored and query vectors are unit-L2 normalized.
    public var cosineSimilarity: Float {
        1 + score / 2
    }
}

public struct SearchDiagnostics: Equatable, Sendable {
    public var dimensionsTouched: UInt64
    public var bytesRead: UInt64
    public var threadsUsed: UInt64
    public var graphSegmentsTraversed: UInt64
    public var graphValidations: UInt64
    public var graphEntrySeedDiscoveries: UInt64
    public var graphVisitedEpochClears: UInt64
    public var graphCandidatesScored: UInt64
    public var graphCandidatesRescored: UInt64
    public var graphSegmentsPrunedByBound: UInt64
}

public struct SearchResult: Equatable, Sendable {
    public var hits: [SearchHit]
    public var generation: UInt64
    public var diagnostics: SearchDiagnostics
}

public struct QueryOptions: Sendable {
    public var k: Int
    public var threadBudget: Int
    public var tier: SearchTier?
    public var graphProfile: GraphProfile
    public var graphEF: Int
    public var graphSeed: UInt64
    public var alpha: Double?
    public var rulesEnabled: Bool
    public var maximumRounds: UInt64?
    public var quotedPhrase: Bool
    public var identifierToken: Bool
    public var rarestExactDocumentFrequency: UInt64?
    public var cancellationToken: ZeppelinCancellationToken?
    public var deadlineNanoseconds: UInt64

    public init(
        k: Int,
        threadBudget: Int = 0,
        tier: SearchTier? = nil,
        graphProfile: GraphProfile = .sift,
        graphEF: Int = 0,
        graphSeed: UInt64 = 0,
        alpha: Double? = nil,
        rulesEnabled: Bool = false,
        maximumRounds: UInt64? = nil,
        quotedPhrase: Bool = false,
        identifierToken: Bool = false,
        rarestExactDocumentFrequency: UInt64? = nil,
        cancellationToken: ZeppelinCancellationToken? = nil,
        deadlineNanoseconds: UInt64 = 0
    ) {
        self.k = k
        self.threadBudget = threadBudget
        self.tier = tier
        self.graphProfile = graphProfile
        self.graphEF = graphEF
        self.graphSeed = graphSeed
        self.alpha = alpha
        self.rulesEnabled = rulesEnabled
        self.maximumRounds = maximumRounds
        self.quotedPhrase = quotedPhrase
        self.identifierToken = identifierToken
        self.rarestExactDocumentFrequency = rarestExactDocumentFrequency
        self.cancellationToken = cancellationToken
        self.deadlineNanoseconds = deadlineNanoseconds
    }
}

public enum QueryMode: Int32, Sendable {
    case vector = 0
    case lexical = 1
    case hybrid = 2
}

public enum FusionMethod: Int32, Sendable {
    case convexCombination = 0
    case reciprocalRank = 1
}

public struct QueryHit: Equatable, Sendable {
    public var documentID: DocumentID?
    public var revision: UInt64?
    public var score: Double
    public var vectorSquaredL2: Double?
    public var lexicalBM25: Double?
}

public struct QueryDiagnostics: Equatable, Sendable {
    public var approximate: Bool
    public var exactRescore: Bool
    public var budgetExhausted: Bool
    public var fusionMethod: FusionMethod?
    public var effectiveAlpha: Double?
    public var fusionRounds: UInt64?
    public var embeddingEpoch: UInt64?
    public var tokenizerEpoch: UInt64?
    public var dimensionsTouched: UInt64
    public var bytesRead: UInt64
    public var documentsEvaluated: UInt64
    public var postingsDecoded: UInt64
}

public struct QueryResult: Equatable, Sendable {
    public var hits: [QueryHit]
    public var generation: UInt64
    public var mode: QueryMode
    public var diagnostics: QueryDiagnostics
}

public struct GenerationReport: Equatable, Sendable {
    public var generation: UInt64
}

public struct PartitionReport: Equatable, Sendable {
    public var generation: UInt64
    public var segmentsDropped: UInt64
    public var bytesReclaimed: UInt64
    public var straddlersSkipped: UInt64
    public var isNoOp: Bool
}

public struct PurgeToken: Equatable, Sendable {
    public var tokenID: UInt64
    public var generation: UInt64
    public var unknownIDCount: UInt64
    public var isNoOp: Bool
}

public struct PurgeReport: Equatable, Sendable {
    public var generation: UInt64
    public var segmentsRewritten: UInt64
    public var unknownIDCount: UInt64
    public var walRewritten: Bool
    public var isNoOp: Bool
}

public enum MaintenanceStatus: Int32, Sendable {
    case complete = 0
    case budgetExhausted = 1
}

public struct MaintenanceReport: Equatable, Sendable {
    public var graphsBuilt: UInt64
    public var bytesConsumed: UInt64
    public var checkpointsResumed: UInt64
    public var status: MaintenanceStatus
}

/// Host-invoked maintenance policy. This value schedules no timers or tasks.
public struct MaintenancePolicy: Sendable {
    public var sealAtActiveRowCount: UInt64
    public var idleInterval: Duration?
    public var wallTimeNanoseconds: UInt64
    public var byteBudget: UInt64

    public init(
        sealAtActiveRowCount: UInt64,
        idleInterval: Duration? = nil,
        wallTimeNanoseconds: UInt64,
        byteBudget: UInt64
    ) {
        self.sealAtActiveRowCount = sealAtActiveRowCount
        self.idleInterval = idleInterval
        self.wallTimeNanoseconds = wallTimeNanoseconds
        self.byteBudget = byteBudget
    }

    @available(macOS 14.0, iOS 17.0, *)
    public func run(
        on store: ZeppelinStore,
        idleFor: Duration = .zero
    ) async throws -> MaintenanceReport {
        let stats = try await store.stats()
        let reachedRowThreshold =
            sealAtActiveRowCount > 0 && stats.activeRowCount >= sealAtActiveRowCount
        let reachedIdleInterval = idleInterval.map { idleFor >= $0 } ?? false
        if stats.activeRowCount > 0 && (reachedRowThreshold || reachedIdleInterval) {
            _ = try await store.seal()
        }
        return try await store.maintain(
            wallTimeNanoseconds: wallTimeNanoseconds,
            bytes: byteBudget
        )
    }
}
