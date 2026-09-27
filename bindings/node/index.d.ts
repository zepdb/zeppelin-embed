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
 * **One namespace only.** Atomicity does not span namespaces. Each namespace
 * has its own write-ahead log, so writes to two stores are two batches, and a
 * crash between them can keep the first and lose the second. Keep documents
 * that must change together in one namespace, or order the writes so that a
 * lost second write is repairable (write dependent documents first and the
 * pointer that makes them live last).
 *
 * **Recovery.** A writable open cuts off a final write that a crash left
 * incomplete. A read-only open never repairs: while such a cut record remains,
 * it throws `ZE_ERR_CORRUPT`. Any other damage to the log fails every open.
 */
export declare class Store {
  constructor(path: string, options?: OpenOptions);
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
   * and `ZE_ERR_ACCESS_MODE` on a read-only store.
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
   * Throws `ZE_ERR_ACCESS_MODE` for read-only stores and `ZE_ERR_BUSY` while
   * a purge is pending. This may rewrite whole segments and blocks the caller.
   * `deleteWhere` matches live documents only: purge earlier deleted IDs
   * explicitly to remove their historical bytes.
   */
  purge(ids: readonly DocumentId[]): PurgeReport;
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
  | 'purgeIntentCorrupt';

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
