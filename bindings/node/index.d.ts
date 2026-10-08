export interface OpenOptions {
  readonly readOnly?: boolean;
  readonly durability?: 'derived' | 'durable' | 'attached';
  readonly commitTier?: 'none' | 'ordered' | 'durable';
  readonly readerDrainTimeoutMs?: bigint;
  readonly maxResidentBytes?: bigint;
  readonly maxTempBytes?: bigint;
  /**
   * Seal automatically. The store seals once at open, which absorbs any WAL
   * tail an earlier session left unsealed, and again before the write that
   * follows this many written documents (upserted, ingested or deleted ids)
   * since the last seal. Smaller values keep each write cheaper; larger values
   * make fewer sealed segments. Disabled when omitted; must be a positive safe
   * integer. Counts documents, not calls or bytes; a batch can overshoot the
   * threshold. Needs a writable store. Combine with autoMerge for consolidation.
   */
  readonly autoSealRows?: number;
  /**
   * Default false. When true, seal and merge once at open, then merge after
   * every automatic or explicit seal. Pair with autoSealRows for maintenance
   * during writes without application calls. Requires a writable store.
   * Synchronous: blocks the caller; errors propagate before the pending write.
   * Native merge requires at least two compatible small scan segments and
   * repeats batches of at most 16 inputs / 8 MiB. Large and graph segments
   * remain separate; this is not a retention policy or a total-store size cap.
   * An explicit seal returns the final merge generation. A failure may follow
   * a committed seal or merge batch; completed maintenance is not rolled back.
   */
  readonly autoMerge?: boolean;
}

/** An actual change completed during this open, not a history of earlier opens.
 * Same from/to versions mean recovery or schema evolution within that format.
 */
export interface Migration {
  readonly kind: 'schema-added' | 'wal-tail-cut' | 'wal-rotated';
  readonly fromFormat: 'manifest/2' | 'wal/1';
  readonly toFormat: 'manifest/2' | 'wal/1';
  readonly generation: bigint;
  readonly description: string;
}

export interface SealReport {
  /** The committed generation; unchanged when there was nothing to seal. */
  readonly generation: bigint;
}

export interface SnapshotReport {
  /** The generation the snapshot captured. Later writes are not in it. */
  readonly generation: bigint;
}

export interface Document {
  readonly id: bigint;
  readonly vector: Float32Array;
  readonly revision?: bigint;
  readonly timestamp?: bigint;
}

export interface MutationReport {
  readonly sequence: bigint;
  readonly generation: bigint;
}

/**
 * An unsigned 128-bit document id: any bigint in `0n..2n ** 128n - 1n`.
 *
 * The engine, its persisted layouts and the C ABI all carry the full 128
 * bits, and every API that returns an id returns the same bigint. That is
 * exactly the UUID range, so a UUID is a document id: convert with
 * `uuidToId` and `idToUuid`. Inputs outside the range throw a `RangeError`
 * with code `ERR_OUT_OF_RANGE`.
 */
export type DocumentId = bigint;

/**
 * The document id of a UUID string: its 32 hex digits read as one
 * big-endian (RFC 9562 byte order) 128-bit integer. Exact for every UUID of
 * any version or variant, and the inverse of `idToUuid`.
 *
 * Accepts only the `8-4-4-4-12` form: 32 ASCII hex digits, either case, with
 * hyphens at offsets 8, 13, 18 and 23 and nothing else (no braces, `urn:`
 * prefix or whitespace). A non-string throws a `TypeError` with code
 * `ERR_INVALID_ARG_TYPE`; any other string throws a `TypeError` with code
 * `ERR_INVALID_ARG_VALUE`.
 */
export declare function uuidToId(uuid: string): DocumentId;

/**
 * The canonical lowercase `8-4-4-4-12` UUID string of a document id, zero
 * padded, so `uuidToId(idToUuid(id)) === id` for every id. A non-bigint
 * throws a `TypeError` with code `ERR_INVALID_ARG_TYPE`; a bigint outside
 * `0n..2n ** 128n - 1n` throws a `RangeError` with code `ERR_OUT_OF_RANGE`.
 */
export declare function idToUuid(id: DocumentId): string;

export type AttributeType =
  | 'u64'
  | 'i64'
  | 'f64'
  | 'bool'
  | 'dictionaryString'
  | 'rawString'
  /** Full unsigned 128-bit ID; equality and set membership are supported. */
  | 'id128';

export interface AttributeDefinition {
  readonly id: number;
  readonly name: string;
  readonly type: AttributeType;
  readonly nullable?: boolean;
}

/** Id128 values are bigints in [0, 2^128 - 1]. Convert UUID strings with
 * uuidToId before writing/filtering and idToUuid after reading. */
export type AttributeValue =
  | { readonly id: number; readonly type: 'id128'; readonly value: DocumentId }
  | { readonly id: number; readonly type: 'null'; readonly value: null }
  | { readonly id: number; readonly type: 'u64'; readonly value: bigint }
  | { readonly id: number; readonly type: 'i64'; readonly value: bigint }
  | { readonly id: number; readonly type: 'f64'; readonly value: number }
  | { readonly id: number; readonly type: 'bool'; readonly value: boolean }
  | { readonly id: number; readonly type: 'string'; readonly value: string };

export interface VectorSpace {
  readonly dimensions: number;
  readonly normalization?: 'none' | 'unitL2';
}

export interface NamespaceSpec {
  /**
   * Persisted tokenizer profile; defaults to textDefault. Supply the same
   * profile on reopen or receive ZE_ERR_EPOCH_MISMATCH. No vector space or
   * embedding epoch is required. voice normalizes transcript number words
   * ("twenty five" matches "25"); code preserves stopwords and skips stemming.
   */
  readonly tokenizerProfile?: 'textDefault' | 'voice' | 'code';
  /**
   * The namespace's typed attributes. Reopening an existing namespace
   * matches them by `id`, in any order: every stored attribute must be
   * declared with the same `name`, `type` and `nullable`. A declared
   * attribute the namespace lacks is added when it is `nullable: true`; the
   * writable open that adds it commits the change, and documents written
   * before it read it as null. Removing or changing an attribute, adding a
   * non-nullable one, or adding one on a `readOnly` open throws
   * `ZE_ERR_SCHEMA_MISMATCH` with a message that names the attribute.
   */
  readonly attributes?: readonly AttributeDefinition[];
  readonly vectorSpace?: VectorSpace;
}

/**
 * A condition on one document's live revision, checked before a write.
 *
 * - A `bigint` requires a live document at exactly that revision.
 * - `null` requires that no live document exists: the id was never written,
 *   or it was deleted.
 * - Omitted or `undefined` sets no condition.
 *
 * The live revision is the `revision` that `get` and `scan` return. The
 * store's single writer checks every condition in the call against the
 * latest committed state, active and sealed, as it was before the call, and
 * does so atomically with applying the call: no other write can land
 * between the check and the write. If any condition fails, the call writes
 * nothing (no log record; the generation does not change) and throws a
 * `ZeppelinError` with code `ZE_ERR_REVISION_CONFLICT`, whose `conflict`
 * names the first failed entry.
 *
 * Compare-and-set on a head document: read `r = get([id]).documents[0]
 * .revision`, then `upsert([{ id, revision: r + 1n, expectedRevision: r }])`;
 * on `ZE_ERR_REVISION_CONFLICT`, read again and retry.
 */
export type ExpectedRevision = bigint | null;

/** The first failed condition of a conditional write. */
export interface RevisionConflict {
  /** Position of the failed entry in the `upsert` or `delete` array. */
  readonly index: number;
  readonly id: DocumentId;
  /** The entry's `expectedRevision`. */
  readonly expectedRevision: ExpectedRevision;
  /** The live revision at the check, or `null` when no live document exists. */
  readonly currentRevision: bigint | null;
}

/** One `delete` entry: a bare id, or an id with a revision condition. */
export type DeleteTarget =
  | DocumentId
  | { readonly id: DocumentId; readonly expectedRevision?: ExpectedRevision };

export interface UpsertDocument {
  readonly id: DocumentId;
  /**
   * The caller-chosen unsigned 64-bit revision; `1n` when omitted. Per id, a
   * revision above the stored one replaces the document. The same revision
   * is an idempotent retry: it succeeds and writes nothing. A lower revision
   * throws `ZE_ERR_STALE_REVISION`. A delete keeps the deleted revision as
   * that floor, so re-creating a deleted id needs a higher revision; the
   * deleted revision itself succeeds and leaves the id deleted.
   */
  readonly revision?: bigint;
  /** Makes the whole call conditional; see `ExpectedRevision`. */
  readonly expectedRevision?: ExpectedRevision;
  readonly timestamp?: bigint;
  readonly vector?: Float32Array;
  readonly text?: string;
  readonly metadata?: Uint8Array;
  readonly attributes?: readonly AttributeValue[];
}

export interface DocumentFields {
  readonly vector?: boolean;
  readonly text?: boolean;
  readonly metadata?: boolean;
  readonly attributes?: boolean;
}

export interface StoredDocument {
  readonly id: DocumentId;
  readonly revision: bigint;
  readonly timestamp: bigint;
  readonly vector?: Float32Array;
  readonly text?: string;
  readonly metadata?: Uint8Array;
  readonly attributes?: AttributeValue[];
}

export interface GetResult {
  readonly documents: Array<StoredDocument | null>;
  readonly missingCount: number;
  readonly generation: bigint;
}

declare const scanCursorBrand: unique symbol;

export interface ScanCursor {
  readonly [scanCursorBrand]: never;
}

export type Filter =
  | {
      readonly op: 'eq' | 'notEq' | 'in' | 'notIn';
      readonly attributeId: number;
      readonly values: readonly AttributeValue[];
    }
  | {
      readonly op: 'range';
      readonly attributeId: number;
      readonly lower?: AttributeValue;
      readonly lowerInclusive?: boolean;
      readonly upper?: AttributeValue;
      readonly upperInclusive?: boolean;
    }
  | { readonly op: 'exists' | 'isNull'; readonly attributeId: number }
  | { readonly op: 'and' | 'or'; readonly children: readonly Filter[] }
  | { readonly op: 'not'; readonly children: readonly [Filter] };

export interface TimestampRange {
  readonly start: bigint;
  readonly end: bigint;
}

/**
 * Orders a scan by a declared `u64`, `i64` or `f64` attribute.
 *
 * Equal values break ties by ascending document id, so pagination is
 * stable. `f64` compares numerically and `-0` equals `+0`. A document whose
 * value is missing or `NaN` sorts after every document with a value, in both
 * directions. An undeclared attribute, attribute 0 (the timestamp), or a
 * `bool` or string attribute throws `ZE_ERR_INVALID_ARGUMENT`.
 */
export interface AttributeScanOrder {
  /** Declared attribute id, an integer in 1..4294967295. */
  readonly attributeId: number;
  readonly direction: 'ascending' | 'descending';
}

export interface ScanRequest {
  /**
   * Continues the scan that returned it. A cursor works only with the order
   * that issued it (a different order, attribute or direction throws
   * `ZE_ERR_INVALID_ARGUMENT`), and any write since that page throws
   * `ZE_ERR_SCAN_STALE`; restart the scan without a cursor.
   */
  readonly cursor?: ScanCursor;
  readonly limit?: number;
  readonly order?:
    | 'storage'
    | 'timestampAscending'
    | 'timestampDescending'
    | AttributeScanOrder;
  readonly fields?: DocumentFields;
  readonly timestampRange?: TimestampRange;
  readonly filter?: Filter;
}

export interface ScanPage {
  readonly documents: StoredDocument[];
  readonly generation: bigint;
  readonly cursor: ScanCursor | null;
}

export interface CountRequest {
  readonly filter?: Filter;
  readonly timestampRange?: TimestampRange;
}

export interface CountResult {
  readonly count: bigint;
  readonly generation: bigint;
}

/** A scheduled purge; tokenId is single-use and belongs to this open Store. */
export interface PurgeTokenReport {
  readonly tokenId: bigint;
  readonly generation: bigint;
  readonly unknownIdCount: bigint;
  readonly isNoOp: boolean;
}

export interface PartitionReport {
  readonly generation: bigint;
  readonly segmentsDropped: bigint;
  /** Immutable file bytes actually reclaimed (excludes WAL bytes). */
  readonly bytesReclaimed: bigint;
  /** Overlapping sealed segments retained because they cross a boundary. */
  readonly straddlersSkipped: bigint;
  readonly isNoOp: boolean;
}

export interface RetentionRequest {
  /** Positive signed-i64 window in the same units as document timestamps. */
  readonly window: bigint;
  /** Caller-supplied signed-i64 current timestamp; no wall clock is read. */
  readonly nowTs: bigint;
}

export interface PurgeReport {
  readonly generation: bigint;
  readonly segmentsRewritten: bigint;
  /** Number of distinct requested IDs absent from physical storage. */
  readonly unknownIdCount: bigint;
  readonly walRewritten: boolean;
  readonly isNoOp: boolean;
}

export interface DeleteWhereReport {
  /** Number of documents deleted; `0n` when nothing matched. */
  readonly deleted: bigint;
  /**
   * Store generation when the call returned. Unchanged when nothing
   * matched.
   */
  readonly generation: bigint;
}

/** The attribute a grouped count groups by. */
export interface CountGroupBy {
  /** A `u64`, `i64`, `dictionaryString` or `rawString` attribute id. */
  readonly attributeId: number;
  /**
   * Most distinct values accepted: an integer in 1..=65536, default 1024.
   * More distinct values throw `ZE_ERR_BUDGET_EXCEEDED`; no group is ever
   * dropped.
   */
  readonly limit?: number;
}

export interface GroupedCountRequest extends CountRequest {
  readonly groupBy: CountGroupBy;
}

export interface CountGroup {
  /** `bigint` for an integer attribute, `string` for a string attribute. */
  readonly value: bigint | string;
  /** Matching live documents with this value; never zero. */
  readonly count: bigint;
}

export interface GroupedCountResult extends CountResult {
  /**
   * Groups in ascending value order: numeric for integers, byte order for
   * strings. Every group comes from `generation`.
   */
  readonly groups: CountGroup[];
  /**
   * Matching documents whose attribute is null. The group counts plus
   * `missingCount` equal `count`.
   */
  readonly missingCount: bigint;
}

export interface SearchOptions {
  readonly k?: number;
  readonly threadBudget?: number;
  readonly tier?: 'auto' | 'exact' | 'scan' | 'graph';
  readonly graphProfile?: 'sift' | 'angular';
  readonly graphEf?: number;
  readonly graphSeed?: bigint;
  readonly deadlineNs?: bigint;
}

export interface SearchHit {
  readonly id: bigint;
  readonly revision: bigint;
  readonly score: number;
}

export type SearchResult = SearchHit[];

/** Which legs a structured query runs, and therefore which mode executes. */
export type QueryMode = 'vector' | 'lexical' | 'hybrid';

export interface QueryRequest {
  /** Document ids allowed on both legs before ranking/fusion; [] selects no hits. Maximum 524288 ids. */
  readonly eligibleIds?: readonly bigint[];
  /** Scan-compatible attribute AST, applied before top-k to every query leg.
   * Unsupported operators, attributes and value types throw instead of being ignored.
   */
  readonly filter?: Filter;
  /** Inclusive start, exclusive end; combined with filter using AND. */
  readonly timestampRange?: TimestampRange;
  /**
   * Query text. Present selects the lexical leg; analysed with the same
   * tokenizer configuration ingest uses.
   */
  readonly text?: string;
  /** Query vector. Present selects the vector leg. */
  readonly vector?: Float32Array;
  /** Requested result count. Defaults to 10. */
  readonly k?: number;
  /** Treat the last analysed term as a type-ahead prefix. */
  readonly lastAsPrefix?: boolean;
  /** Explicit convex-combination fusion weight in 0..=1. */
  readonly alpha?: number;
  /** Enable the query-shape alpha rules. Ignored when `alpha` is set. */
  readonly rulesEnabled?: boolean;
  /** Fusion widening round cap; 0n materialises full lists immediately. */
  readonly maxRounds?: bigint;
  /** The query contains a quoted phrase. */
  readonly quotedPhrase?: boolean;
  /** Token classification found an identifier. */
  readonly identifierToken?: boolean;
  /** Lowest exact-token document frequency. */
  readonly rarestExactDocumentFrequency?: bigint;
  /** No value lets the engine pick, which is distinct from `'auto'`. */
  readonly tier?: 'auto' | 'exact' | 'scan' | 'graph';
  readonly graphProfile?: 'sift' | 'angular';
  readonly graphEf?: number;
  readonly graphSeed?: bigint;
  /** 0 selects every detected physical performance core. */
  readonly threadBudget?: number;
  /** Relative monotonic deadline in nanoseconds; 0n means none. */
  readonly deadlineNs?: bigint;
  readonly cancelToken?: CancellationToken;
  /**
   * Opt in to a {@link Snippet} on each hit, and set the excerpt length in
   * UTF-8 bytes: an integer from 1 to 4294967295. The excerpt may run up to 3
   * bytes longer to finish a character. Requires `text`; a vector-only query
   * with `snippetBytes` throws `ZE_ERR_INVALID_ARGUMENT`. A value that is not
   * such an integer throws a `RangeError` (`ERR_OUT_OF_RANGE`), and a
   * non-number a `TypeError`. Without it the query reads no extra text.
   */
  readonly snippetBytes?: number;
}

/**
 * A matched range in {@link Snippet.text}, in UTF-16 code units, so
 * `snippet.text.slice(start, end)` is the matched text.
 */
export interface SnippetHighlight {
  /** Inclusive absolute UTF-8 byte offset in the document's stored source text. */
  readonly sourceByteStart: number;
  /** Exclusive absolute UTF-8 byte offset; use Buffer.from(source).subarray(start, end). */
  readonly sourceByteEnd: number;
  /** Inclusive start. */
  readonly start: number;
  /** Exclusive end. */
  readonly end: number;
}

/**
 * An excerpt of a hit's stored text with the ranges the query matched.
 *
 * The engine finds the ranges with the same analyzer and the same scored terms
 * the query used: stemming, case and accent folding, and every term a
 * `lastAsPrefix` prefix expanded to. The query operators are terms and a
 * trailing prefix, so a quoted phrase marks each of its terms. A document has
 * one text field, so a hit has at most one snippet.
 *
 * The excerpt starts at a matched token; of the windows starting at each
 * match, the one with the most distinct matches wins, then the most matches,
 * then the earliest, so snippets are deterministic. No ellipsis is inserted.
 */
export interface Snippet {
  /** Inclusive absolute UTF-8 byte offset of this excerpt in the stored source text. */
  readonly sourceByteStart: number;
  /** Exclusive absolute UTF-8 byte offset of this excerpt in the stored source text. */
  readonly sourceByteEnd: number;
  /** The excerpt. */
  readonly text: string;
  /**
   * Matched ranges, ascending and non-overlapping. Only ranges wholly inside
   * the excerpt are reported, so a `snippetBytes` shorter than a matched word
   * can leave this empty.
   */
  readonly highlights: SnippetHighlight[];
  /** The excerpt starts after the start of the stored text. */
  readonly truncatedStart: boolean;
  /** The excerpt ends before the end of the stored text. */
  readonly truncatedEnd: boolean;
}

export interface QueryHit {
  readonly id: bigint;
  /** Absent on a fused hit, which carries identity only. */
  readonly revision?: bigint;
  /** Larger-is-better ranking score of the executed mode. */
  readonly score: number;
  /** Squared L2 distance of the vector leg, when it ran. */
  readonly vectorSquaredL2?: number;
  /** BM25 score of the lexical leg, when it ran. */
  readonly lexicalBm25?: number;
  /**
   * Present when the request set `snippetBytes` and the hit's stored text
   * contains a query term. Every hit with a positive `lexicalBm25` has one; it
   * is absent on a hybrid hit whose `lexicalBm25` is zero.
   */
  readonly snippet?: Snippet;
}

export interface QueryFusion {
  readonly method: 'convex' | 'reciprocalRank';
  readonly effectiveAlpha: number;
  readonly rounds: bigint;
}

export interface QueryResult {
  readonly hits: QueryHit[];
  /** The pinned store generation queried. */
  readonly generation: bigint;
  readonly mode: QueryMode;
  /** Some candidate membership came from a non-exhaustive path. */
  readonly approximate: boolean;
  /** Every returned score came from full-precision rows. */
  readonly exactRescore: boolean;
  /** An execution budget fired. */
  readonly budgetExhausted: boolean;
  /** Present only when both legs ran and their results were fused. */
  readonly fusion?: QueryFusion;
}

/**
 * A cancellation token a query can be asked to observe.
 *
 * Close every token; the handle is owned by the engine and is not released by
 * garbage collection.
 */
export declare class CancellationToken {
  constructor();
  cancel(): void;
  close(): void;
}

export declare class ZeppelinError extends Error {
  constructor(message: string, code: string, errorCode: number);
  readonly code: string;
  readonly errorCode: number;
  /** Present when `code` is `ZE_ERR_REVISION_CONFLICT`. */
  readonly conflict?: RevisionConflict;
  /** Graph operations preserve settlement status even on failure. */
  readonly disposition?: GraphDisposition;
  readonly generation?: bigint | null;
  readonly admittedGeneration?: bigint | null;
  readonly changedGeneration?: bigint | null;
}

export declare class UnsupportedPlatformError extends Error {
  constructor(platform: string, arch: string);
  readonly code: 'ERR_ZEPPELIN_UNSUPPORTED_PLATFORM';
}

/**
 * Thrown when the platform and architecture are supported but this package
 * ships no binary for the running runtime, such as an Electron major it was
 * not built for, or a Node-API version older than 8.
 */
export declare class UnsupportedRuntimeError extends Error {
  constructor(detail: string);
  readonly code: 'ERR_ZEPPELIN_UNSUPPORTED_RUNTIME';
}

/**
 * A store is one namespace. Its writes are atomic batches.
 *
 * **Batch atomicity.** One `upsert`, `ingest` or `delete` call is one batch,
 * whatever its size. If the process is killed or the machine stops at any
 * point during the call, the next writable open shows every document of the
 * batch or none of them; it never shows part of a batch. Every batch whose
 * call returned is kept (within the durability the open options select: the
 * default `derived` mode and the `none` tier survive a process kill but not a
 * power cut). Put documents that must change together, such as a note head
 * and its body, in one call.
 *
 * These methods commit one namespace. Use `namespaceBatch` for an explicit
 * cross-namespace mutation after closing participating writable handles.
 *
 * **Recovery.** A writable open cuts off a final write that a crash left
 * incomplete. A read-only open never repairs: while such a cut record remains,
 * it throws `ZE_ERR_CORRUPT`. Any other damage to the log fails every open.
 */
export declare class Store {
  graphSetMaintenancePolicy(options: {readonly autoReclaim?: boolean; readonly reclaimAfterBytes?: number}): void;
  graphMaintain(options?: GraphControlOptions): GraphMaintenanceReport;
  graphMaintainAsync(options?: GraphAsyncOptions): Promise<GraphMaintenanceReport>;
  static graphSupported(): boolean;
  enableGraph(): { readonly generation: bigint };
  graphResources(): GraphResources;
  graphApply(items: readonly GraphMutation[], options?: GraphControlOptions): GraphResult;
  graphApplyAsync(items: readonly GraphMutation[], options?: GraphAsyncOptions): Promise<GraphResult>;
  cypher(text: string, parameters?: Readonly<Record<string, GraphParameter>>, options?: GraphQueryOptions): GraphResult;
  cypherAsync(text: string, parameters?: Readonly<Record<string, GraphParameter>>, options?: GraphQueryOptions & GraphAsyncOptions): Promise<GraphResult>;
  graphQuery(plan: GraphPlan, options?: GraphPlanQueryOptions): GraphResult;
  graphQueryAsync(plan: GraphPlan, options?: GraphPlanQueryOptions & GraphAsyncOptions): Promise<GraphResult>;
  graphGetNodes(ids: readonly bigint[], options?: GraphNodeFields & GraphControlOptions): (GraphNode | null)[];
  graphGetNodesAsync(ids: readonly bigint[], options?: GraphNodeFields & GraphAsyncOptions): Promise<(GraphNode | null)[]>;
  graphGetRelationships(ids: readonly bigint[], options?: GraphControlOptions): (GraphRelationship | null)[];
  graphGetRelationshipsAsync(ids: readonly bigint[], options?: GraphAsyncOptions): Promise<(GraphRelationship | null)[]>;

  constructor(path: string, options?: OpenOptions | StoreRelationshipOptions);
  /** Opens on a native worker, including recovery and auto-seal/auto-merge. */
  static openAsync(path: string, options?: OpenOptions): Promise<Store>;
  /** Async variants run engine work on native workers. Input parsing and result
   * conversion run on JS. Async mutations are ordered per Store; await a mutation
   * before reads or synchronous calls that depend on it. Inputs are copied.
   * close() remains synchronous: admitted work may finish, queued work rejects
   * with ZE_ERR_CLOSED. It may wait for admitted engine work to drain.
   * All failures (including validation) reject with the sync API's error types.
   */
  upsertAsync(documents: readonly UpsertDocument[]): Promise<MutationReport>;
  /** AbortSignal cancels engine work cooperatively and rejects with
   * ZeppelinError (ZE_ERR_CANCELLED), never partial results. The promise settles
   * after the worker stops. Use either signal or cancelToken, not both; keep a
   * caller-owned cancelToken open until settlement. A completion may win an
   * abort race. */
  queryAsync(request: QueryRequest & { readonly signal?: AbortSignal }): Promise<QueryResult>;
  /** Same cancellation contract as queryAsync; one complete page or an error. */
  scanAsync(request?: ScanRequest & { readonly signal?: AbortSignal }): Promise<ScanPage>;
  /** Off-thread seal, including configured auto-merge. */
  /** Prepare lexical assembly and prefix vocabulary; mutations invalidate them. */
  warmLexical(options?: { readonly deadlineNs?: bigint }): void;
  /** Prepare off the event loop; signal and nonzero deadlineNs are exclusive. */
  warmLexicalAsync(options?: { readonly signal?: AbortSignal; readonly deadlineNs?: bigint }): Promise<void>;
  sealAsync(): Promise<SealReport>;
  /** Off-thread merge of sealed segments. */
  mergeAsync(): Promise<SealReport>;
  /** Alias for mergeAsync. */
  maintainAsync(): Promise<SealReport>;
  /** Off-thread purge; wait defaults to true. */
  purgeAsync(ids: readonly DocumentId[], options?: { readonly wait?: true }): Promise<PurgeReport>;
  purgeAsync(ids: readonly DocumentId[], options: { readonly wait: false }): Promise<PurgeTokenReport>;
  purgeAsync(ids: readonly DocumentId[], options: { readonly wait?: boolean }): Promise<PurgeReport | PurgeTokenReport>;
  /** Off-thread physical purge completion. */
  awaitPurgeAsync(tokenId: bigint): Promise<PurgeReport>;
  /** Alias for snapshot(), which already runs off-thread. Close cancels an
   * admitted snapshot with ZE_ERR_CANCELLED; a queued snapshot may be CLOSED. */
  snapshotAsync(target: string): Promise<SnapshotReport>;
  /** Alias for snapshotAsync. */
  backupAsync(target: string): Promise<SnapshotReport>;
  /** Changes completed by open, including its optional auto-seal. Empty for an
   * unchanged or read-only open. Supported Node baseline: 0.4.2. Manifest v1
   * predates Node releases and is refused with ZE_ERR_FORMAT_VERSION; newer
   * unsupported formats fail with ZE_ERR_FORMAT_TOO_NEW. No epoch is inferred.
   */
  readonly migrations: readonly Migration[];
  /** Rebuilds every sealed text index from stored text with the current tokenizer.
   * Seals active writes first; preserves IDs, revisions, vectors and metadata.
   * Requires a writer. Missing text needed by existing postings fails loudly.
   * Retained embedding epochs are preserved; a different retained tokenizer
   * epoch returns ZE_ERR_EPOCH_MISMATCH instead of reinterpreting its text.
   * Empty stores are a no-op. On publication failure, close and reopen before
   * retrying. Sealing and reindex publication are separate durable commits.
   */
  reindexText(): SealReport;
  /** Writes `documents` as one atomic batch; see the class notes. */
  ingest(documents: readonly Document[], dimension: number): MutationReport;
  /**
   * Upserts the documents as one atomic batch (see the class notes). If any
   * `expectedRevision` condition fails, nothing is written and the call
   * throws `ZE_ERR_REVISION_CONFLICT`.
   */
  upsert(documents: readonly UpsertDocument[]): MutationReport;
  get(ids: readonly DocumentId[], fields?: DocumentFields): GetResult;
  /**
   * Deletes the ids as one atomic batch (see the class notes). An entry can
   * carry an `expectedRevision`
   * condition; if any condition fails, nothing is deleted and the call
   * throws `ZE_ERR_REVISION_CONFLICT`.
   */
  delete(ids: readonly DeleteTarget[]): MutationReport;
  /**
   * Deletes every document whose current version matches `filter`, in one
   * mutation, and removes their bytes from disk.
   *
   * The filter is the same structured `Filter` that `scan` and `count` take
   * and is required; an absent filter throws `ERR_MISSING_ARGS`, and a
   * filter that does not fit the namespace attributes throws
   * `ZE_ERR_INVALID_ARGUMENT`. No other write can land between finding the
   * matches and deleting them, and readers see every matched document or
   * none of them.
   *
   * Reclamation bound: when the call returns, no byte of a deleted document
   * (text, attributes, metadata, vector) remains in any file of the store:
   * each affected sealed segment and the write-ahead log are rewritten
   * without it. There is no separate compaction step and no knob. The cost
   * is one rewrite of each segment that held a match, plus the unsealed
   * rows. If the process stops during the call, either nothing was deleted
   * or the next writable open finishes the removal before it returns; a
   * read-only open leaves that pending removal to the next writable open.
   *
   * Throws `ZE_ERR_BUSY` while an earlier physical purge is still pending,
   * `ZE_ERR_STORE_BUSY` before mutation while snapshots are open, and
   * `ZE_ERR_ACCESS_MODE` on a read-only store.
   */
  deleteWhere(filter: Filter): DeleteWhereReport;
  /**
   * Synchronously removes all stored versions of these IDs, including text
   * left by an earlier `delete`. IDs may be live or already deleted; unknown
   * IDs are reported. Requires a nonempty array (`ZE_ERR_EMPTY_BATCH`).
   *
   * Returns only after affected segments and the WAL are rewritten and old
   * files unlinked. If interrupted after scheduling, the next writable open
   * completes the purge. On failure, close and reopen before retrying.
   * Throws `ZE_ERR_ACCESS_MODE` for read-only stores, `ZE_ERR_BUSY` while
   * a purge is pending, and `ZE_ERR_STORE_BUSY` before scheduling while
   * snapshots are open. This may rewrite whole segments and blocks the caller.
   * `deleteWhere` matches live documents only: purge earlier deleted IDs
   * explicitly to remove their historical bytes.
   */
  purge(ids: readonly DocumentId[]): PurgeReport;
  /** Persisted user attributes, excluding the built-in timestamp column. Copies names and types. */
  schema(): AttributeDefinition[];
  purge(ids: readonly DocumentId[], options?: { readonly wait?: true }): PurgeReport;
  /**
   * Schedule removal without rewriting artifacts yet. Pass tokenId to
   * awaitPurge on this same open Store. Conflicting operations reject while
   * pending; reopening completes pending work but invalidates the old token.
   */
  purge(ids: readonly DocumentId[], options: { readonly wait: false }): PurgeTokenReport;
  purge(ids: readonly DocumentId[], options: { readonly wait?: boolean }): PurgeReport | PurgeTokenReport;
  /**
   * Synchronously complete physical removal, blocking the calling thread.
   * Consumes the unsigned-u64 tokenId from purge(ids, {wait:false}); unknown,
   * consumed or foreign-handle tokens throw ZE_ERR_INVALID_ARGUMENT.
   * On failure, close and reopen before retrying.
   */
  awaitPurge(tokenId: bigint): PurgeReport;
  /**
   * Synchronously drop whole sealed segments contained in [start, end).
   * Bounds are signed-i64 bigints and start must be less than end. Straddlers,
   * unstamped segments and unsealed data remain; this is not per-document expiry.
   * Inspect bytesReclaimed: the engine can report a committed drop even when
   * unlinking an orphan fails. Use purge(ids) for a physical-removal guarantee.
   */
  dropPartition(range: TimestampRange): PartitionReport;
  /**
   * Apply dropPartition to timestamps below saturating(nowTs - window),
   * using the same whole-segment selection and cleanup semantics. Explicit,
   * synchronous and blocking; no automatic scheduling or background timer.
   */
  applyRetention(request: RetentionRequest): PartitionReport;
  scan(request?: ScanRequest): ScanPage;
  /**
   * Count live documents matching the optional filter and timestamp range.
   * With `groupBy`, also count per attribute value at one generation; an
   * `f64`, `bool` or unknown attribute throws `ZE_ERR_INVALID_ARGUMENT`.
   */
  count(request: GroupedCountRequest): GroupedCountResult;
  count(request?: CountRequest): CountResult;
  searchFiltered(
    vector: Float32Array,
    filter: Filter,
    options?: SearchOptions,
  ): SearchResult;
  search(vector: Float32Array, k: number): SearchHit[];
  /**
   * One structured query: `text` runs the lexical leg, `vector` the vector
   * leg, and both together run hybrid fusion. A request with neither is
   * rejected.
   */
  query(request: QueryRequest): QueryResult;
  /**
   * Seal the active segment into an immutable segment and absorb the WAL
   * records it covers, so a later open does not replay them. After the seal
   * commits, the WAL file is replaced by an empty one (a 40-byte header), so
   * it holds only writes made since the last seal; a crash at any point of
   * that replacement loses nothing. An empty active segment is a no-op.
   * Repeated upsert/seal cycles are supported, including across close/open.
   */
  seal(): SealReport;
  /**
   * Merge small sealed scan segments during application idle time. Synchronous:
   * blocks writers, repeats atomic batches of at most 16 inputs and 8 MiB of
   * input files. Decoded working memory is larger than the input-byte bound.
   * Large and graph segments remain separate. Enable autoMerge alongside
   * autoSealRows to run this automatically after each seal.
   * Open snapshots defer input-file deletion for both explicit and automatic
   * merges. Close the snapshots and reopen the writer to reclaim retired files.
   * Active writes and WAL are unchanged; call seal() first to include them.
   * Returns the final generation, unchanged if no compatible batch fits.
   * Requires a writable store. A failure may follow completed atomic batches.
   */
  merge(): { readonly generation: bigint };
  /**
   * Write a consistent snapshot of the store at one generation into
   * `target`, for a backup or an export. The copy runs on a worker thread,
   * so the application keeps reading and writing through this store; writers
   * wait only while the generation is pinned, which copies no document
   * bytes. Writes, seals and segment rewrites made while it runs are absent
   * from the snapshot.
   *
   * `target` must not exist or must be an empty directory, its parent must
   * exist, and it must not be the store directory or inside it; otherwise
   * the promise rejects with `ZE_ERR_INVALID_ARGUMENT` and nothing is
   * written. A read-only store rejects with `ZE_ERR_ACCESS_MODE`, a closed
   * one with `ZE_ERR_CLOSED`, and a non-string target with a `TypeError`.
   * While a physical purge is pending it rejects with `ZE_ERR_UNSUPPORTED`.
   * Closing the store cancels a running snapshot (`ZE_ERR_CANCELLED`). The
   * snapshot is written under a hidden temporary name and renamed into place
   * only once every file is synced, so a failure or crash never leaves a
   * partial snapshot at `target`.
   *
   * The snapshot is an ordinary store directory: open it, read-only or
   * read-write, with `openNamespace` and the same spec to restore the
   * captured state.
   */
  snapshot(target: string): Promise<SnapshotReport>;
  /**
   * Pins this writable handle's current generation without copying files.
   * Reads and scan cursors stay stable while the source changes or closes.
   * The returned Store is read-only; call close() to release its pin.
   * While pinned, purge() and deleteWhere() return ZE_ERR_STORE_BUSY before mutation.
   * Retired segment files are reclaimed on the next writable open. If the
   * source closes first, reopening a writer is busy until all views close.
   * Admission uses the existing handle and does not reacquire a namespace
   * writer lock. Opening a second writable namespace handle still returns Busy.
   */
  openSnapshot(): Store;

  close(): void;
}

export declare const ABI_VERSION: number;

export declare function openNamespace(
  root: string,
  name: string,
  spec: NamespaceSpec,
  options?: OpenOptions,
): Store;
/** Off-thread namespace open, with the same options and migrations. */
export declare function openNamespaceAsync(
  root: string,
  name: string,
  spec: NamespaceSpec,
  options?: OpenOptions,
): Promise<Store>;

export declare function listNamespaces(root: string): string[];

/**
 * What is damaged. Kinds are append-only.
 *
 * - `manifestMissing`: `manifest.ze` is absent, but the WAL or segment files
 *   prove a committed snapshot existed; data it covered is unreachable.
 * - `manifestCorrupt`: the manifest failed its checksum or decoder.
 * - `manifestAheadOfWal`: the manifest covers WAL records the WAL lacks.
 * - `segmentMissing`: a segment the manifest references does not exist.
 * - `segmentCorrupt`: a segment header, length, identity or trailer is bad.
 * - `segmentMismatch`: a segment header disagrees with the manifest.
 * - `segmentRegionCorrupt`: a segment region failed its checksum; `offset`
 *   is the region's byte offset and `detail` names the region.
 * - `segmentIndexInvalid`: a checksum-valid region (columns, alive set, text
 *   index, stored text or metadata, graph) failed its decoder or disagrees
 *   with the segment's row count.
 * - `walMissing`: the WAL is absent although the manifest covers records.
 * - `walHeaderCorrupt`: the WAL file header is truncated or invalid.
 * - `walRecordCorrupt`: a WAL record failed framing, checksum or sequence
 *   validation, including a torn tail; `offset` is the record's offset.
 * - `walRecordInvalid`: a checksum-valid WAL record cannot be replayed.
 * - `unreadable`: a store file exists but could not be read.
 * - `purgeIntentCorrupt`: the pending purge intent `purge.ze` failed to
 *   decode. A pending intent that decodes is not damage: the next writable
 *   open completes that purge.
 */
export type VerifyFindingKind =
  | 'manifestMissing'
  | 'manifestCorrupt'
  | 'manifestAheadOfWal'
  | 'segmentMissing'
  | 'segmentCorrupt'
  | 'segmentMismatch'
  | 'segmentRegionCorrupt'
  | 'segmentIndexInvalid'
  | 'walMissing'
  | 'walHeaderCorrupt'
  | 'walRecordCorrupt'
  | 'walRecordInvalid'
  | 'unreadable'
  | 'purgeIntentCorrupt'
  | 'graphObjectMissing'
  | 'graphObjectCorrupt'
  | 'graphInventoryInvalid';

export interface VerifyFinding {
  readonly kind: VerifyFindingKind;
  /** File name relative to the store directory. */
  readonly file: string;
  /** Byte offset of the damage inside `file`, when the decoder knows it. */
  readonly offset?: bigint;
  /** Human-readable decoder detail, for logs and support reports. */
  readonly detail: string;
}

export interface VerifyReport {
  /** True exactly when `findings` is empty. */
  readonly ok: boolean;
  /** Generation of the decoded manifest; `0n` without one. */
  readonly generation: bigint;
  /** Segments the manifest references. */
  readonly segmentsChecked: bigint;
  /** WAL records that passed checksum and sequence validation. */
  readonly walRecordsChecked: bigint;
  /** Every damaged artifact, in walk order: manifest, segments, WAL. */
  readonly findings: VerifyFinding[];
}

/**
 * Verify one store directory end to end without opening or modifying it.
 *
 * Walks the manifest, every referenced segment (header, region checksums,
 * file trailer, and every index region's decoder, with columns read against
 * the manifest schema), the WAL through the same replay recovery runs, and
 * any pending purge intent. It creates, writes, renames, locks and removes
 * nothing, so it is safe to call after an unclean shutdown and before
 * reopening. For a namespace pass `path.join(root, name)`.
 *
 * Damage is returned as findings, never thrown. Every finding is damage: the
 * engine refuses to open the store or would lose data from it. A torn WAL
 * tail is damage, because recovery refuses it. Segment files no manifest
 * references and leftover temporary files are not findings; a writable open
 * removes them without losing data.
 *
 * Throws `ZeppelinError` `ZE_ERR_NOT_FOUND` when the path does not exist,
 * `ZE_ERR_IO` when it is not a directory, and `ZE_ERR_INVALID_ARGUMENT` for
 * an empty path or one containing a NUL character; a `TypeError` when `path`
 * is not a string. Verifying a store another process is writing can report a
 * write that is in flight.
 */
export declare function verify(path: string): VerifyReport;

/**
 * Opens an existing diagnostics copy read-only without a caller-declared epoch
 * or schema. Uses the persisted identity; tokenizer incompatibility and corrupt
 * data still fail. Never repairs, creates, seals or writes; mutations throw
 * ZE_ERR_ACCESS_MODE. Close the returned store when finished. Use verify(path)
 * if corruption prevents opening. Inspect a quiescent copy, not a live writer.
 */
export declare function openInspection(path: string): Store;
/** Graph scalar inputs: bigint is signed I64; finite number is F64. */
export type GraphScalar = null | boolean | bigint | number | string;
/** Numeric typed arrays follow scalar rules: numbers are F64, bigints are I64. */
export type GraphParameter = GraphScalar | readonly GraphParameter[] |
  Int8Array | Uint8Array | Uint8ClampedArray | Int16Array | Uint16Array |
  Int32Array | Uint32Array | Float32Array | Float64Array | BigInt64Array | BigUint64Array;
/** Stored lists contain one non-null scalar type (empty lists are allowed). */
export type GraphProperty = GraphScalar | readonly boolean[] | readonly bigint[] | readonly number[] | readonly string[];
export type GraphProperties = Readonly<Record<string, GraphProperty>>;
export type GraphDisposition = 'NotApplicable' | 'NotCommitted' | 'Committed' | 'Replayed' | 'NoOp' | 'Indeterminate';

export interface GraphNode {
  readonly vector?: Float32Array;
  readonly kind: 'node';
  /** Store-local unsigned 128-bit identity. */
  readonly id: bigint;
  /** Absent for unkeyed nodes created by Cypher. */
  readonly namespace?: string;
  readonly key?: string;
  readonly revision: bigint;
  readonly lastChangeGeneration: bigint;
  readonly labels: readonly string[];
  readonly properties: GraphProperties;
  /** Present only when selected by the engine; use ze.stored_text(n) to read text. */
  readonly text?: string;
}
export interface GraphRelationship {
  readonly kind: 'relationship';
  readonly id: bigint;
  readonly source: bigint;
  readonly target: bigint;
  readonly type: string;
  readonly namespace?: string;
  readonly key?: string;
  readonly revision: bigint;
  readonly lastChangeGeneration: bigint;
  readonly properties: GraphProperties;
}
/** Query lists can contain mixed scalar/entity values and nested lists. */
export type GraphValue = GraphScalar | GraphNode | GraphRelationship | readonly GraphValue[];
export interface GraphReceipt {
  readonly item: number;
  readonly kind: 'node' | 'relationship';
  readonly id: bigint;
  readonly disposition: GraphDisposition;
  readonly deleted: boolean;
  readonly revision: bigint;
  readonly generation: bigint;
}
export interface GraphResult {
  readonly disposition: GraphDisposition;
  /** Changed generation when present, otherwise admitted generation; null if neither is known. */
  readonly generation: bigint | null;
  readonly admittedGeneration: bigint | null;
  readonly changedGeneration: bigint | null;
  readonly columns: readonly string[];
  readonly rows: readonly (readonly GraphValue[])[];
  /** One receipt per apply item in input order; empty for read queries. */
  readonly receipts: readonly GraphReceipt[];
}
/** A directed child -> parent edge type's immutable incoming-reference policy. */
export interface GraphRelationshipType {
  readonly type: string;
  /**
   * restrict refuses deletion while a surviving child references the parent.
   * cascade deletes dependent source nodes transitively, including cycles.
   * Both apply to apply() and Cypher DELETE/DETACH DELETE, atomically.
   * Explicitly deleting an edge releases its dependency; any remaining restrict
   * edge from outside the deletion set rejects the entire mutation.
   */
  readonly onDelete: 'restrict' | 'cascade';
}
export interface GraphMaintenanceReport {
  readonly generation: bigint;
  readonly replacedPhysicalRefs: bigint;
  readonly newPackBytes: bigint;
  readonly relocatedBytes: bigint;
  readonly drainedPacks: bigint;
  readonly reclaimedBytes: bigint;
  readonly removedBytes: bigint;
  readonly cycleComplete: boolean;
}
export type GraphQueryOptions = GraphControlOptions & {
  /**
   * Returned-row cap: integer 0..65536. Omitted or 0 selects 1024.
   * Exceeding it throws ZeppelinError with ZE_ERR_BUDGET_EXCEEDED; never truncates.
   * Other engine memory/work limits still apply.
   */
  readonly maxRows?: number;
}
/** Existing node ID or a zero-based node item in this same atomic batch. */
export type GraphEndpoint = bigint | { readonly local: number };
interface GraphKeyedMutation {
  readonly namespace: string;
  readonly key: string;
  /** Positive unsigned 64-bit revision. Exact keyed retries return Replayed. */
  readonly revision: bigint;
}
type GraphWriteOperation =
  | { readonly operation: 'create' }
  | { readonly operation: 'put'; readonly expectedId: bigint }
  | { readonly operation: 'recreate'; readonly expectedDeletionRevision: bigint };
/** Full replacement image: omitted labels/properties/text become empty/absent. Vectors and document fields use the Store epoch. */
export type GraphNodeWrite = GraphKeyedMutation & (
  | { readonly operation: 'create'; readonly id?: bigint }
  | { readonly operation: 'put'; readonly expectedId: bigint; readonly id?: never }
  | { readonly operation: 'recreate'; readonly expectedDeletionRevision: bigint; readonly id?: never }
) & {
  readonly kind: 'node';
  readonly labels?: readonly string[];
  readonly properties?: GraphProperties;
  readonly text?: string;
  readonly vector?: Float32Array;
  readonly timestamp?: bigint;
  readonly attributes?: readonly AttributeValue[];
  readonly metadata?: Uint8Array;
};
export type GraphRelationshipWrite = GraphKeyedMutation & GraphWriteOperation & {
  readonly kind: 'relationship';
  readonly type: string;
  readonly source: GraphEndpoint;
  readonly target: GraphEndpoint;
  readonly properties?: GraphProperties;
};
export type GraphDelete = GraphKeyedMutation & {
  readonly operation: 'delete';
  readonly expectedId: bigint;
} & ({ readonly kind: 'node'; readonly detach?: boolean } | { readonly kind: 'relationship' });
export type GraphMutation = GraphNodeWrite | GraphRelationshipWrite | GraphDelete;
/** One participant; operations run in the listed phase order. */
export interface NamespaceMutation {
  readonly name: string;
  /** Must match the existing namespace, including tokenizer. No schema evolution. */
  readonly spec: NamespaceSpec;
  readonly upserts?: readonly UpsertDocument[];
  readonly deletes?: readonly bigint[];
  /** Evaluated after upserts and explicit deletes, in the private prepared state. */
  readonly deleteWhere?: Filter;
}
/**
 * Fully durable atomic mutation over 2..128 existing namespaces of one root.
 * Close their writable handles and openSnapshot views first; either refuses
 * the batch, including a view whose source writer has already closed.
 * Returns each namespace's generation in input order. Revision conditions on
 * upserts apply to the original participant state, before its explicit deletes.
 *
 * New opens (including read-only or direct-path opens) select all of a committed
 * participant's changes without waiting for any sibling to recover. Readers
 * opened before commit retain their old snapshot. Invalid mutations publish none.
 * An I/O error at commit can mean either outcome: reopen before retrying.
 *
 * This first version copies each participating store and retains old stores and
 * abandoned preparations. Deletes are logical; deleted bytes may remain in those
 * copies. Keep the root intact; use Store.snapshot for an independent export.
 * Missing/corrupt transaction records fail loudly. Single-namespace writes issue
 * no root transaction I/O; namespace open resolves the root decision.
 */
export declare function namespaceBatch(root: string, participants: readonly NamespaceMutation[]): bigint[];

/** A mutation using a same-process writable namespace handle. */
export interface LiveNamespaceMutation extends NamespaceMutation {
  readonly store: Store;
}
/**
 * Commits through live writable handles; returns generations in input order.
 * Handles remain usable on success. A commit I/O error may have committed:
 * close and reopen every participant before retrying. Deletion and snapshot
 * retention follow the core live-batch protocol.
 */
export declare function namespaceBatchLive(root: string, participants: readonly LiveNamespaceMutation[]): bigint[];

/** An existing namespace with its exact schema/epoch/tokenizer declaration. */
export interface CascadeParticipant {
  readonly name: string;
  readonly spec: NamespaceSpec;
}
/** Rule owned by the child namespace, referring to participants by array index. */
export interface CascadeDeclaration {
  readonly parentIndex: number;
  readonly childIndex: number;
  /** Child attribute must have type id128; null values have no parent. */
  readonly attributeId: number;
}
/**
 * Durably declares a cascade between existing namespaces of one root. Supply the
 * parent and child specs (one participant for a self-cycle, which is rejected).
 * Exact redeclaration is idempotent. Multiple ownership rules are allowed; a
 * child is deleted when any declared parent reference is deleted. Cycles throw
 * ZE_ERR_CASCADE_CYCLE with the namespace path in the error message.
 *
 * Close participant writers and their openSnapshot views first. Rules survive
 * reopen in root metadata; keep the root intact. This is a declaration for
 * deleteCascade, not foreign-key validation on upsert or implicit behavior for
 * Store.delete, Store.deleteWhere or namespaceBatch. No rule removal API yet.
 */
export declare function declareCascade(root: string, participants: readonly CascadeParticipant[], declaration: CascadeDeclaration): void;
/** One namespace's explicit IDs; dependants are discovered by the engine. */
export interface CascadeDeleteParticipant extends CascadeParticipant {
  readonly deletes?: readonly DocumentId[];
}
/**
 * Deletes parent IDs and every transitive dependant in ONE namespaceBatch-style
 * atomic mutation. Supply 1..128 participants with the exact existing specs,
 * including every namespace reachable through declared cascades. Omission fails
 * before publication. IDs are unsigned 128-bit bigints; use uuidToId for UUID strings.
 * Returns generations in participant order. Upserts and filters are rejected.
 *
 * Close all participant writers and openSnapshot views first. Existing readers
 * keep their snapshots; newly opened stores resolve the shared root decision.
 * An I/O error near commit is indeterminate: reopen before retrying. Whole stores
 * are copied; old stores and abandoned preparations remain. Deletes are logical:
 * bounded reclamation or physical erasure of deleted text is NOT provided.
 */
export declare function deleteCascade(root: string, participants: readonly CascadeDeleteParticipant[]): bigint[];

export type GraphControlOptions =
  | { readonly deadlineNs?: bigint; readonly cancelToken?: never }
  | { readonly deadlineNs?: never; readonly cancelToken?: CancellationToken };
export type GraphAsyncOptions =
  | { readonly signal: AbortSignal; readonly deadlineNs?: never; readonly cancelToken?: never }
  | (GraphControlOptions & { readonly signal?: never });
export interface GraphNodeFields { readonly text?: boolean; readonly vector?: boolean }
export interface GraphResources { readonly engineBytes: bigint; readonly enginePeakBytes: bigint; readonly applicationBytes: bigint; readonly applicationPeakBytes: bigint }
export interface GraphProjection { readonly slot: number; readonly expression: number }
export interface GraphSortKey { readonly expression: number; readonly descending: boolean }
export interface GraphExpansion { readonly source: number; readonly node: number; readonly relationship: number; readonly direction: number; readonly types: readonly string[]; readonly pattern: number }
export interface GraphEdgePredicate { readonly slot: number; readonly expression: number }
export type GraphExpression =
  | readonly ['literal', GraphParameter]
  | readonly ['slot' | 'parameter', number]
  | readonly ['unary', number, number]
  | readonly ['binary', number, number, number]
  | readonly ['property' | 'hasLabel', number, string]
  | readonly ['list', readonly number[]]
  | readonly ['aggregate', number, number | null, boolean];
export type GraphPlanMutation =
  | readonly ['createNode', number, readonly string[]]
  | readonly ['createRelationship', number, number, number, string]
  | readonly ['removeProperty', number, string]
  | readonly ['setLabel', number, string, boolean]
  | readonly ['delete', number, boolean]
  | readonly ['setProperty', number, string, number];
export type GraphOperator =
  | readonly ['unit']
  | readonly ['join' | 'optionalApply', number, number, number | null]
  | readonly ['distinct' | 'eager' | 'collect', number]
  | readonly ['sort', number, readonly GraphSortKey[]]
  | readonly ['scanNodes', number, string | null]
  | readonly ['mutate', number, readonly GraphPlanMutation[]]
  | readonly ['aggregate', number, readonly GraphProjection[], readonly GraphProjection[]]
  | readonly ['search', number, number | null]
  | readonly ['offsetLimit', number, bigint, bigint | null]
  | readonly ['lookupNode' | 'lookupRelationship', number, bigint]
  | readonly ['lookupKey', number, number, string, number]
  | readonly ['expand', number, GraphExpansion]
  | readonly ['boundedExpand', number, GraphExpansion, number, number, GraphEdgePredicate | null]
  | readonly ['project' | 'with', number, readonly GraphProjection[]]
  | readonly ['filter', number, number]
  | readonly ['eligibleSet', number, number, number];
export interface GraphSearchOptions {
  readonly profile: number; readonly ef: number; readonly seed: bigint;
  readonly lastAsPrefix: boolean; readonly rescore: number; readonly alpha: number | null;
  readonly rulesEnabled: boolean; readonly maxRounds: bigint | null;
}
export interface GraphSearch {
  readonly kind: readonly ['vector' | 'text', number] | readonly ['hybrid', number, number];
  readonly call: number; readonly k: number; readonly tier?: number | null;
  readonly eligibleSet?: number | null; readonly window?: number | null;
  readonly node: number; readonly score: number;
  readonly vectorDistance?: number | null; readonly lexicalScore?: number | null;
  readonly options?: GraphSearchOptions | null;
}
export interface GraphPlan {
  readonly root: number; readonly operators: readonly GraphOperator[];
  readonly expressions: readonly GraphExpression[];
  readonly parameters: readonly {readonly name: string; readonly kinds: number}[];
  readonly searches: readonly GraphSearch[]; readonly eagerSearches: readonly number[];
}

/** Synchronous creation only; other open options are refused in this form. */
export interface StoreRelationshipOptions {
  readonly relationshipTypes: readonly GraphRelationshipType[];
  readonly maxResidentBytes?: bigint;
  readonly readerDrainTimeoutMs?: bigint;
}

export type GraphPlanQueryOptions = GraphControlOptions & { readonly parameters?: Readonly<Record<string, GraphParameter>> };
