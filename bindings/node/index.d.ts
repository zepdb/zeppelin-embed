export interface OpenOptions {
  readonly readOnly?: boolean;
  readonly durability?: 'derived' | 'durable' | 'attached';
  readonly commitTier?: 'none' | 'ordered' | 'durable';
  readonly readerDrainTimeoutMs?: bigint;
  readonly maxResidentBytes?: bigint;
  readonly maxTempBytes?: bigint;
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

export type DocumentId = bigint;

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
  readonly attributes?: readonly AttributeDefinition[];
  readonly vectorSpace?: VectorSpace;
}

export interface UpsertDocument {
  readonly id: DocumentId;
  readonly revision?: bigint;
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

export interface ScanRequest {
  readonly cursor?: ScanCursor;
  readonly limit?: number;
  readonly order?: 'storage' | 'timestampAscending' | 'timestampDescending';
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
  upsert(documents: readonly UpsertDocument[]): MutationReport;
  get(ids: readonly DocumentId[], fields?: DocumentFields): GetResult;
  delete(ids: readonly DocumentId[]): MutationReport;
  scan(request?: ScanRequest): ScanPage;
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
