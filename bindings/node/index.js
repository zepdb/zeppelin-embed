'use strict';

const path = require('node:path');

class ZeppelinError extends Error {
  constructor(message, code, errorCode) {
    super(message);
    this.name = 'ZeppelinError';
    this.code = code;
    this.errorCode = errorCode;
  }
}

class UnsupportedPlatformError extends Error {
  constructor(platform, arch) {
    super(
      `@zepdb/zeppelin-embed supports macOS on Apple silicon and Intel, and ` +
        `Windows x64; received ${platform}/${arch}`,
    );
    this.name = 'UnsupportedPlatformError';
    this.code = 'ERR_ZEPPELIN_UNSUPPORTED_PLATFORM';
  }
}

class UnsupportedRuntimeError extends Error {
  constructor(detail) {
    super(`@zepdb/zeppelin-embed does not ship a binary for this runtime: ${detail}`);
    this.name = 'UnsupportedRuntimeError';
    this.code = 'ERR_ZEPPELIN_UNSUPPORTED_RUNTIME';
  }
}

/**
 * Electron major versions this package actually ships a binary for.
 *
 * The package `os`/`cpu` fields describe a cross-product and cannot express
 * which platform/runtime pairs were built and qualified, so the loader decides
 * it explicitly. Selection is deterministic: the exact binary is chosen from
 * `process.platform`, `process.arch` and `process.versions.electron`, and an
 * unsupported combination is refused by name. There is deliberately no
 * try-each-and-catch fallback, which would turn a packaging mistake into a
 * confusing runtime failure somewhere else.
 */
const SUPPORTED_ELECTRON_MAJORS = new Set([44]);

/** The lowest Node-API version the prebuilt addons are compiled against. */
const REQUIRED_NAPI_VERSION = 8;

function resolveBindingPath() {
  const { platform, arch } = process;
  const prebuilds = path.join(__dirname, 'prebuilds');

  // macOS ships one binary per architecture and no per-runtime variant. The
  // addon is a bundle with `-undefined dynamic_lookup`, so its Node-API
  // symbols resolve from the host process, and the same file loads in Node and
  // in Electron. Windows below needs the runtime distinction because its addon
  // delay-loads `node.exe`.
  if (platform === 'darwin') {
    if (arch === 'arm64') {
      return path.join(prebuilds, 'darwin-arm64', 'zeppelin_embed.node');
    }
    if (arch === 'x64') {
      return path.join(prebuilds, 'darwin-x64', 'zeppelin_embed.node');
    }
  }

  if (platform === 'win32' && arch === 'x64') {
    const electron = process.versions.electron;
    if (electron !== undefined) {
      const major = Number.parseInt(electron.split('.')[0], 10);
      if (!SUPPORTED_ELECTRON_MAJORS.has(major)) {
        throw new UnsupportedRuntimeError(
          `Electron ${electron}; this package ships Electron ` +
            `${[...SUPPORTED_ELECTRON_MAJORS].join(', ')} binaries for win32-x64`,
        );
      }
      return path.join(prebuilds, 'win32-x64', `electron-${major}`, 'zeppelin_embed.node');
    }

    const napi = Number.parseInt(process.versions.napi ?? '0', 10);
    if (!Number.isFinite(napi) || napi < REQUIRED_NAPI_VERSION) {
      throw new UnsupportedRuntimeError(
        `Node-API ${process.versions.napi ?? 'unknown'}; this package requires ` +
          `Node-API ${REQUIRED_NAPI_VERSION} or later`,
      );
    }
    return path.join(prebuilds, 'win32-x64', 'node-napi8', 'zeppelin_embed.node');
  }

  throw new UnsupportedPlatformError(platform, arch);
}

const binding = require(resolveBindingPath());

function translateError(error) {
  if (
    error instanceof Error &&
    typeof error.code === 'string' &&
    error.code.startsWith('ZE_') &&
    typeof error.errorCode === 'number'
  ) {
    const translated = new ZeppelinError(error.message, error.code, error.errorCode);
    // A failed revision condition names the document the caller must re-read.
    if (error.conflict !== undefined) translated.conflict = error.conflict;
    return translated;
  }
  return error;
}

function callNative(callback) {
  try {
    return callback();
  } catch (error) {
    throw translateError(error);
  }
}

/**
 * The native token handle, kept off the public shape so a token is used
 * through its methods rather than by reading a bigint out of it.
 */
const nativeToken = Symbol('zeppelin.cancelToken');

/**
 * A cancellation token that a query can be asked to observe.
 *
 * The C ABI's token is a generation-tagged handle with an explicit lifecycle,
 * so this owns that handle: `cancel` asks any query holding it to stop, and
 * `close` releases it. Freeing is not left to garbage collection, because the
 * engine reuses generations and a token released at an unpredictable time
 * would be a handle whose validity the caller cannot reason about.
 */
class CancellationToken {
  constructor() {
    this[nativeToken] = callNative(() => binding.createCancelToken());
    this._closed = false;
  }

  cancel() {
    if (this._closed) {
      throw new ZeppelinError('cancellation token is closed', 'ZE_ERR_CLOSED', 0);
    }
    callNative(() => binding.cancelToken(this[nativeToken]));
  }

  close() {
    if (this._closed) return;
    this._closed = true;
    callNative(() => binding.freeCancelToken(this[nativeToken]));
  }
}

/**
 * Reads and validates the `autoSealRows` open option before any handle is
 * opened, so a bad value never leaks a native store.
 */
function autoSealRowsOption(options) {
  const rows = options?.autoSealRows;
  if (rows === undefined) return 0;
  if (!Number.isSafeInteger(rows) || rows < 1) {
    throw new RangeError('autoSealRows must be a positive safe integer');
  }
  if (options.readOnly === true) {
    throw new RangeError('autoSealRows needs a writable store; readOnly is true');
  }
  return rows;
}

/**
 * Attaches a freshly opened native handle and applies the auto-seal policy.
 *
 * With `autoSealRows` set, the store seals once at open, which absorbs any WAL
 * tail an earlier session left unsealed, and again before the write that
 * would follow `autoSealRows` written documents. Sealing before the write
 * rather than after it means an error always reports a write that did not
 * happen, never one that did.
 */
function attach(store, open, autoSealRows) {
  store._native = callNative(open);
  store._autoSealRows = autoSealRows;
  store._unsealedWrites = 0;
  if (autoSealRows > 0) {
    try {
      store.seal();
    } catch (error) {
      try {
        store._native.close();
      } catch {
        // The seal error is the one to report.
      }
      throw error;
    }
  }
  return store;
}

class Store {
  constructor(storePath, options = {}) {
    attach(
      this,
      () => new binding.NativeStore(storePath, options),
      autoSealRowsOption(options),
    );
  }

  _write(count, write) {
    if (this._autoSealRows > 0 && this._unsealedWrites >= this._autoSealRows) {
      this.seal();
    }
    const report = callNative(write);
    this._unsealedWrites += count;
    return report;
  }

  ingest(documents, dimension) {
    return this._write(documents?.length ?? 0, () =>
      this._native.ingest(documents, dimension),
    );
  }

  upsert(documents) {
    return this._write(documents?.length ?? 0, () => this._native.upsert(documents));
  }

  get(ids, fields) {
    return callNative(() => this._native.get(ids, fields));
  }

  delete(ids) {
    return this._write(ids?.length ?? 0, () => this._native.delete(ids));
  }

  /**
   * Deletes every document matching `filter` in one mutation and removes
   * their bytes from every store file before returning. The deleted count
   * joins the auto-seal row count once it is known.
   */
  deleteWhere(filter) {
    const report = this._write(0, () => this._native.deleteWhere(filter));
    this._unsealedWrites += Number(report.deleted);
    return report;
  }

  purge(ids) {
    return callNative(() => this._native.purge(ids));
  }

  schema() {
    return callNative(() => this._native.schema());
  }

  scan(request) {
    return callNative(() => this._native.scan(request));
  }

  count(request) {
    return callNative(() => this._native.count(request));
  }

  searchFiltered(vector, filter, options) {
    return callNative(() => this._native.searchFiltered(vector, filter, options));
  }

  search(vector, k) {
    return callNative(() => this._native.search(vector, k));
  }

  /**
   * One structured query: a vector leg, a lexical leg, or hybrid fusion of
   * both.
   *
   * Which legs run is the engine's rule, not a second policy here: `text`
   * selects the lexical leg, `vector` the vector leg, and both together
   * select fusion. `filter` and `timestampRange` constrain both legs before
   * ranking, using the same semantics as scan(). A `cancelToken` is unwrapped to the native handle so the
   * caller passes the token object rather than a bare bigint.
   */
  query(request) {
    const token = request?.cancelToken;
    const native =
      token === undefined || token === null
        ? request
        : { ...request, cancelToken: token[nativeToken] ?? token };
    return callNative(() => this._native.query(native));
  }

  /**
   * Seals the active segment into an immutable segment and absorbs the WAL
   * prefix it covers, so a later open does not replay those writes. An empty
   * active segment is a no-op that returns the current generation.
   */
  seal() {
    const report = callNative(() => this._native.seal());
    this._unsealedWrites = 0;
    return report;
  }

  /**
   * Writes a consistent snapshot of the store into `target` on a worker
   * thread and resolves to `{ generation }`, the generation it captured.
   * Writes made while it runs are absent from the snapshot. Every failure,
   * including a bad argument, is a rejection.
   */
  async snapshot(target) {
    try {
      return await this._native.snapshot(target);
    } catch (error) {
      throw translateError(error);
    }
  }

  close() {
    return callNative(() => this._native.close());
  }
}

/** The only accepted UUID spelling: 8-4-4-4-12 ASCII hex, either case. */
const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_DOCUMENT_ID = (1n << 128n) - 1n;

function argumentError(ErrorType, code, message) {
  const error = new ErrorType(message);
  error.code = code;
  return error;
}

/**
 * The document id of a UUID: its 32 hex digits as one big-endian 128-bit
 * integer. Document ids are full 128-bit values end to end, so this is exact
 * for every UUID and the inverse of `idToUuid`. The returned bigint also
 * supplies id128 attribute values and equality/membership filter values.
 */
function uuidToId(uuid) {
  if (typeof uuid !== 'string') {
    throw argumentError(TypeError, 'ERR_INVALID_ARG_TYPE', 'uuid must be a string');
  }
  if (!UUID_PATTERN.test(uuid)) {
    throw argumentError(
      TypeError,
      'ERR_INVALID_ARG_VALUE',
      'uuid must be 8-4-4-4-12 hexadecimal digits',
    );
  }
  return BigInt(`0x${uuid.replaceAll('-', '')}`);
}

/** The canonical lowercase UUID string of a document id. */
function idToUuid(id) {
  if (typeof id !== 'bigint') {
    throw argumentError(TypeError, 'ERR_INVALID_ARG_TYPE', 'document id must be a bigint');
  }
  if (id < 0n || id > MAX_DOCUMENT_ID) {
    throw argumentError(
      RangeError,
      'ERR_OUT_OF_RANGE',
      'document id must be an unsigned 128-bit bigint',
    );
  }
  const hex = id.toString(16).padStart(32, '0');
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20),
  ].join('-');
}

// The native namespace parser validates and persists spec.tokenizerProfile.
function openNamespace(root, name, spec, options = {}) {
  return attach(
    Object.create(Store.prototype),
    () => new binding.NativeStore(root, options, name, spec),
    autoSealRowsOption(options),
  );
}

function openInspection(storePath) {
  return new Store(storePath, { readOnly: true, inspection: true });
}

function listNamespaces(root) {
  return callNative(() => binding.listNamespaces(root));
}

/**
 * Walks one store directory read-only and reports every damaged manifest,
 * segment, and WAL artifact. It needs no open store and writes nothing, so it
 * is safe to run after an unclean shutdown, before reopening.
 */
function verify(storePath) {
  return callNative(() => binding.verify(storePath));
}

module.exports = {
  ABI_VERSION: binding.abiVersion,
  CancellationToken,
  Store,
  UnsupportedPlatformError,
  UnsupportedRuntimeError,
  ZeppelinError,
  idToUuid,
  listNamespaces,
  openNamespace,
  openInspection,
  uuidToId,
  verify,
};
