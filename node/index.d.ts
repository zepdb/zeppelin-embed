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

export interface SearchHit {
  readonly id: bigint;
  readonly revision: bigint;
  readonly score: number;
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

export declare class Store {
  constructor(path: string, options?: OpenOptions);
  ingest(documents: readonly Document[], dimension: number): MutationReport;
  search(vector: Float32Array, k: number): SearchHit[];
  close(): void;
}

export declare const ABI_VERSION: number;
