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
   * make fewer sealed segments. Needs a writable store.
   */
  readonly autoSealRows?: number;
}

export interface SealReport {
  /** The committed generation; unchanged when there was nothing to seal. */
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
  | 'rawString';

export interface AttributeDefinition {
  readonly id: number;
  readonly name: string;
  readonly type: AttributeType;
  readonly nullable?: boolean;
}

export type AttributeValue =
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

export declare class Store {
  constructor(path: string, options?: OpenOptions);
  ingest(documents: readonly Document[], dimension: number): MutationReport;
  /**
   * Upserts the documents as one batch. If any `expectedRevision` condition
   * fails, nothing is written and the call throws `ZE_ERR_REVISION_CONFLICT`.
   */
  upsert(documents: readonly UpsertDocument[]): MutationReport;
  get(ids: readonly DocumentId[], fields?: DocumentFields): GetResult;
  /**
   * Deletes the ids as one batch. An entry can carry an `expectedRevision`
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
   * and `ZE_ERR_ACCESS_MODE` on a read-only store.
   */
  deleteWhere(filter: Filter): DeleteWhereReport;
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
   * records it covers, so a later open does not replay them. An empty active
   * segment is a no-op.
   */
  seal(): SealReport;
  close(): void;
}

export declare const ABI_VERSION: number;

export declare function openNamespace(
  root: string,
  name: string,
  spec: NamespaceSpec,
  options?: OpenOptions,
): Store;

export declare function listNamespaces(root: string): string[];
