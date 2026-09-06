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

export declare class ZeppelinError extends Error {
  constructor(message: string, code: string, errorCode: number);
  readonly code: string;
  readonly errorCode: number;
}

export declare class UnsupportedPlatformError extends Error {
  constructor(platform: string, arch: string);
  readonly code: 'ERR_ZEPPELIN_UNSUPPORTED_PLATFORM';
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
