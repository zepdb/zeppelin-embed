'use strict';

const path = require('node:path');
const os = require('node:os');

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
    for (const key of ['disposition', 'generation', 'admittedGeneration', 'changedGeneration']) {
      if (error[key] !== undefined) translated[key] = error[key];
    }
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

function autoMergeOption(options) {
  const enabled = options?.autoMerge;
  if (enabled === undefined) return false;
  if (typeof enabled !== 'boolean') {
    throw new TypeError('autoMerge must be a boolean');
  }
  if (enabled && options.readOnly === true) {
    throw new RangeError('autoMerge needs a writable store; readOnly is true');
  }
  return enabled;
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
function attach(store, open, autoSealRows, autoMerge) {
  store._native = callNative(open);
  store._autoSealRows = autoSealRows;
  store._autoMerge = autoMerge;
  store._unsealedWrites = 0;
  try {
    const report = callNative(() => store._native.openMigrations());
    const migrations = [];
    const add = (kind, format, generation, description) => migrations.push(Object.freeze({
      kind, fromFormat: format, toFormat: format, generation, description,
    }));
    if (report.changes & 2) add('wal-tail-cut', 'wal/1', report.generation, 'Removed an incomplete final WAL record.');
    if (report.changes & 1) add('schema-added', 'manifest/2', report.generation, 'Committed added nullable attributes; existing rows read null.');
    if (autoSealRows > 0 || autoMerge) {
      const sealed = callNative(() => store._native.seal());
      if (sealed.generation > report.generation) add('wal-rotated', 'wal/1', sealed.generation, 'Sealed replayed writes and rotated the absorbed WAL to a header.');
      if (autoMerge) store.merge();
    }
    Object.defineProperty(store, 'migrations', {value: Object.freeze(migrations), enumerable: true});
  } catch (error) {
    try { store._native.close(); } catch { /* Preserve the open error. */ }
    throw error;
  }
  return store;
}

// The token is kept alive until the native worker has actually stopped. Merely
// racing its promise against an abort would leave work running in the engine.
async function withSignal(signal, run) {
  if (signal === undefined) return run(undefined);
  if (!(signal instanceof AbortSignal)) throw new TypeError('signal must be an AbortSignal');
  const token = new CancellationToken();
  const cancel = () => token.cancel();
  try {
    signal.addEventListener('abort', cancel, { once: true });
    if (signal.aborted) cancel();
    return await run(token[nativeToken]);
  } finally {
    signal.removeEventListener('abort', cancel);
    token.close();
  }
}

async function attachAsync(storePath, options, name, spec) {
  const autoSealRows = autoSealRowsOption(options);
  const autoMerge = autoMergeOption(options);
  let native;
  try {
    native = await (name === undefined ? binding.openAsync(storePath, options)
      : binding.openAsync(storePath, options, name, spec));
    const store = Object.create(Store.prototype);
    store._native = native;
    store._autoSealRows = autoSealRows;
    store._autoMerge = autoMerge;
    store._unsealedWrites = 0;
    const report = native.openMigrations();
    const migrations = [];
    const add = (kind, format, generation, description) => migrations.push(Object.freeze({
      kind, fromFormat: format, toFormat: format, generation, description,
    }));
    if (report.changes & 2) add('wal-tail-cut', 'wal/1', report.generation, 'Removed an incomplete final WAL record.');
    if (report.changes & 1) add('schema-added', 'manifest/2', report.generation, 'Committed added nullable attributes; existing rows read null.');
    if (autoSealRows > 0 || autoMerge) {
      const sealed = await native.sealAsync();
      if (sealed.generation > report.generation) add('wal-rotated', 'wal/1', sealed.generation, 'Sealed replayed writes and rotated the absorbed WAL to a header.');
      if (autoMerge) await native.mergeAsync();
    }
    Object.defineProperty(store, 'migrations', { value: Object.freeze(migrations), enumerable: true });
    return store;
  } catch (error) {
    if (native) { try { native.close(); } catch { /* Preserve open failure. */ } }
    throw translateError(error);
  }
}

async function openNamespaceAsync(root, name, spec, options = {}) {
  return attachAsync(root, options, name, spec);
}

class Store {
  static async openAsync(storePath, options = {}) {
    return attachAsync(storePath, options);
  }

  // Serialize async mutations so auto-seal accounting follows committed writes.
  _queueWrite(run) {
    const result = (this._asyncWrites ?? Promise.resolve()).then(run);
    this._asyncWrites = result.catch(() => {});
    return result.catch(error => { throw translateError(error); });
  }

  async upsertAsync(documents) {
    const owned = structuredClone(documents);
    return this._queueWrite(async () => {
      if (this._autoSealRows > 0 && this._unsealedWrites >= this._autoSealRows) {
        await this._native.sealAsync();
        if (this._autoMerge) await this._native.mergeAsync();
        this._unsealedWrites = 0;
      }
      const report = await this._native.upsertAsync(owned);
      this._unsealedWrites += owned?.length ?? 0;
      return report;
    });
  }

  async queryAsync(request) {
    try {
      if (request?.signal !== undefined && request?.cancelToken != null) {
        throw new TypeError('use signal or cancelToken, not both');
      }
      return await withSignal(request?.signal, token => this._native.queryAsync({
        ...request, cancelToken: token ?? request?.cancelToken?.[nativeToken] ?? request?.cancelToken ?? 0n,
      }));
    } catch (error) { throw translateError(error); }
  }

  async scanAsync(request = {}) {
    try {
      return await withSignal(request.signal, token => this._native.scanAsync({ ...request, cancelToken: token ?? 0n }));
    } catch (error) { throw translateError(error); }
  }

  warmLexical(options = {}) {
    try { return this._native.warmLexical({ deadlineNs: options.deadlineNs ?? 0n }); }
    catch (error) { throw translateError(error); }
  }

  async warmLexicalAsync(options = {}) {
    try {
      return await withSignal(options.signal, token => this._native.warmLexicalAsync({ cancelToken: token ?? 0n, deadlineNs: options.deadlineNs ?? 0n }));
    } catch (error) { throw translateError(error); }
  }

  async sealAsync() {
    return this._queueWrite(async () => {
      const report = await this._native.sealAsync();
      const result = this._autoMerge ? await this._native.mergeAsync() : report;
      this._unsealedWrites = 0;
      return result;
    });
  }
  async mergeAsync() { return this._queueWrite(() => this._native.mergeAsync()); }
  async maintainAsync() { return this.mergeAsync(); }
  async purgeAsync(ids, options = {}) {
    const owned = structuredClone(ids);
    const ownedOptions = { ...options };
    return this._queueWrite(() => this._native.purgeAsync(owned, ownedOptions));
  }
  async awaitPurgeAsync(tokenId) { return this._queueWrite(() => this._native.awaitPurgeAsync(tokenId)); }
  async snapshotAsync(target) { return this.snapshot(target); }
  async backupAsync(target) { return this.snapshot(target); }

  constructor(storePath, options = {}) {
    attach(
      this,
      () => new binding.NativeStore(storePath, options),
      autoSealRowsOption(options),
      autoMergeOption(options),
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

  awaitPurge(tokenId) {
    return callNative(() => this._native.awaitPurge(tokenId));
  }

  dropPartition(range) {
    return callNative(() => this._native.dropPartition(range));
  }

  applyRetention(policy) {
    return callNative(() => this._native.applyRetention(policy));
  }

  purge(ids, options = {}) {
    return callNative(() => this._native.purge(ids, options));
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
   * select fusion. With `snippetBytes`, excerpts and highlights include absolute
   * sourceByteStart/sourceByteEnd (UTF-8); highlight start/end remain UTF-16.
   * `eligibleIds`, `filter` and `timestampRange` constrain both legs before
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
  reindexText() {
    const report = callNative(() => this._native.reindexText());
    this._unsealedWrites = 0;
    return report;
  }

  seal() {
    const report = callNative(() => this._native.seal());
    const finalReport = this._autoMerge ? this.merge() : report;
    this._unsealedWrites = 0;
    return finalReport;
  }

  /** Merge small sealed segments synchronously during application idle time. */
  merge() {
    return callNative(() => this._native.merge());
  }

  openSnapshot() {
    return attach(Object.create(Store.prototype), () => this._native.openSnapshot(), 0, false);
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
    autoMergeOption(options),
  );
}

function openInspection(storePath) {
  return new Store(storePath, { readOnly: true, inspection: true });
}

function declareCascade(root, participants, declaration) {
  return callNative(() => binding.declareCascade(root, participants, declaration));
}

function deleteCascade(root, participants) {
  return callNative(() => binding.deleteCascade(root, participants));
}

function namespaceBatchLive(root, participants) {
  if (!Array.isArray(participants)) throw new TypeError('participants must be an array');
  const nativeParticipants = participants.map(participant => {
    if (!(participant.store instanceof Store)) throw new TypeError('participant store must be a Store');
    return { ...participant, store: participant.store._native };
  });
  return callNative(() => binding.namespaceBatchLive(root, nativeParticipants));
}

function namespaceBatch(root, participants) {
  return callNative(() => binding.namespaceBatch(root, participants));
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


function graphInvalid(message) {
  const error = new ZeppelinError(message, 'ZE_ERR_INVALID_ARGUMENT', 1);
  error.disposition = 'NotCommitted';
  throw error;
}
function graphObject(value, allowed, field) {
  if (value === null || typeof value !== 'object' || Array.isArray(value) ||
      ![Object.prototype, null].includes(Object.getPrototypeOf(value))) graphInvalid(`${field} must be a plain object`);
  for (const key of Reflect.ownKeys(value)) {
    if (typeof key !== 'string' || (allowed && !allowed.includes(key))) graphInvalid(`unknown ${field} field: ${String(key)}`);
    if (!Object.getOwnPropertyDescriptor(value, key).hasOwnProperty('value')) graphInvalid(`${field} must not contain accessors`);
  }
}
function graphString(value, field) {
  if (typeof value !== 'string') graphInvalid(`${field} must be a string`);
  // Reject unpaired UTF-16 surrogates instead of silently replacing input bytes.
  for (let i = 0; i < value.length; i++) {
    const c = value.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const next = value.charCodeAt(++i);
      if (!(next >= 0xdc00 && next <= 0xdfff)) graphInvalid(`${field} must be valid Unicode`);
    } else if (c >= 0xdc00 && c <= 0xdfff) graphInvalid(`${field} must be valid Unicode`);
  }
}
function graphUnsigned(value, bits, field) {
  if (typeof value !== 'bigint' || value <= 0n || value >= (1n << BigInt(bits))) graphInvalid(`${field} must be a positive ${bits}-bit bigint`);
}
function graphScalar(value) {
  if (value === null || typeof value === 'boolean') return;
  if (typeof value === 'string') return graphString(value, 'value');
  if (typeof value === 'bigint' && value >= -(1n << 63n) && value < (1n << 63n)) return;
  if (typeof value === 'number' && Number.isFinite(value)) return;
  graphInvalid('values must be null, boolean, signed 64-bit bigint, finite number or string');
}
// Match graph_abi/values.rs: 16 list levels and 524288 children per request.
function graphParameter(value, state, depth = 0) {
  if (Array.isArray(value) || (ArrayBuffer.isView(value) && !(value instanceof DataView))) {
    if (depth >= 16) graphInvalid('parameter list depth exceeds 16 (or contains a cycle)');
    state.elements += value.length;
    if (state.elements > 524288) graphInvalid('parameter lists exceed 524288 elements');
    for (const element of value) graphParameter(element, state, depth + 1);
    return;
  }
  try { graphScalar(value); } catch (error) {
    if (error instanceof ZeppelinError) graphInvalid(`unsupported parameter list/scalar value: ${error.message}`);
    throw error;
  }
}
function graphProperties(properties) {
  if (properties === undefined) return;
  graphObject(properties, null, 'properties');
  for (const [key, value] of Object.entries(properties)) {
    graphString(key, 'property name');
    if (Array.isArray(value)) {
      for (const element of value) {
        graphScalar(element);
        if (element === null || typeof element !== typeof value[0]) graphInvalid('property lists must be homogeneous non-null scalars');
      }
    } else graphScalar(value);
  }
}
function graphEndpoint(value) {
  if (typeof value === 'bigint') return graphUnsigned(value, 128, 'endpoint');
  graphObject(value, ['local'], 'endpoint');
  if (!Number.isInteger(value.local) || value.local < 0 || value.local > 0xffffffff) graphInvalid('local endpoint must be an unsigned item index');
}
const graphConstruction = Symbol('graph construction');
class GraphStore {
  #native;
  constructor(token, native) {
    if (token !== graphConstruction) throw new TypeError('use GraphStore.open(path, options)');
    this.#native = native;
  }
  static isSupported() {
    // Darwin 23 is macOS 14; the Rust constructors enforce the same floor.
    return binding.graphSupported === true &&
      (process.platform !== 'darwin' || Number.parseInt(os.release(), 10) >= 23);
  }
  static open(storePath, options = {}) {
    if (!GraphStore.isSupported()) throw new ZeppelinError('graph requires macOS 14 or newer, or Windows x64', 'ZE_ERR_UNSUPPORTED', 11);
    graphString(storePath, 'path');
    graphObject(options, ['mode', 'maxResidentBytes', 'readerDrainTimeoutMs', 'relationshipTypes', 'autoReclaim', 'reclaimAfterBytes'], 'open options');
    const mode = options.mode ?? 'create';
    if (!['create', 'readWrite', 'readOnly'].includes(mode)) graphInvalid('unknown graph open mode');
    const autoReclaim = options.autoReclaim === undefined ? true : options.autoReclaim;
    const reclaimAfterBytes = options.reclaimAfterBytes === undefined ? 67108864 : options.reclaimAfterBytes;
    if (typeof autoReclaim !== 'boolean') graphInvalid('autoReclaim must be boolean');
    if (!Number.isSafeInteger(reclaimAfterBytes) || reclaimAfterBytes < 1048576) graphInvalid('reclaimAfterBytes must be a safe integer of at least 1048576');
    if (mode === 'readOnly' && (options.autoReclaim !== undefined || options.reclaimAfterBytes !== undefined)) graphInvalid('maintenance options require a writable graph');
    const relationshipTypes = options.relationshipTypes === undefined ? [] : options.relationshipTypes;
    if (options.relationshipTypes !== undefined && mode !== 'create') graphInvalid('relationshipTypes can only be declared at creation');
    if (!Array.isArray(relationshipTypes) || relationshipTypes.length > 16384) graphInvalid('relationshipTypes must be an array of at most 16384 rules');
    const names = new Set();
    for (const rule of relationshipTypes) {
      graphObject(rule, ['type', 'onDelete'], 'relationship type');
      graphString(rule.type, 'relationship type');
      if (!['restrict', 'cascade'].includes(rule.onDelete)) graphInvalid('onDelete must be restrict or cascade');
      if (names.has(rule.type)) graphInvalid('duplicate relationship type declaration');
      names.add(rule.type);
    }
    const maxResidentBytes = options.maxResidentBytes ?? 268435456;
    const readerDrainTimeoutMs = options.readerDrainTimeoutMs ?? 250;
    if (!Number.isSafeInteger(maxResidentBytes) || maxResidentBytes < 1 || maxResidentBytes > 268435456) graphInvalid('maxResidentBytes must be in 1..268435456');
    if (!Number.isSafeInteger(readerDrainTimeoutMs) || readerDrainTimeoutMs < 0) graphInvalid('readerDrainTimeoutMs must be a nonnegative safe integer');
    // Construct through a private token so the native handle cannot be supplied by a caller.
    const native = callNative(() => binding.graphOpen(storePath, ['create', 'readWrite', 'readOnly'].indexOf(mode), maxResidentBytes, readerDrainTimeoutMs, relationshipTypes));
    try {
      if (mode !== 'readOnly') callNative(() => binding.graphSetMaintenancePolicy(native, autoReclaim, reclaimAfterBytes));
      return GraphStore.#create(native);
    } catch (error) {
      callNative(() => binding.graphClose(native));
      throw error;
    }
  }
  static #create(native) {
    return new GraphStore(graphConstruction, native);
  }
  // Native close consumes the handle even when its final checkpoint reports an error.
  close() { return callNative(() => binding.graphClose(this.#native)); }
  maintain() { return callNative(() => binding.graphMaintain(this.#native)); }
  async maintainAsync() {
    try { return await binding.graphMaintainAsync(this.#native); } catch (error) { throw translateError(error); }
  }
  apply(items) { return this.#apply(items, false); }
  async applyAsync(items) {
    try { return await this.#apply(items, true); } catch (error) { throw translateError(error); }
  }
  #apply(items, async) {
    if (!Array.isArray(items) || items.length > 16384) graphInvalid('items must be an array of at most 16384 mutations');
    for (const item of items) {
      graphObject(item, ['kind', 'operation', 'namespace', 'key', 'revision', 'expectedId', 'expectedDeletionRevision', 'detach', 'labels', 'properties', 'text', 'type', 'source', 'target'], 'item');
      if (!['node', 'relationship'].includes(item.kind)) graphInvalid('kind must be node or relationship');
      if (!['create', 'put', 'delete', 'recreate'].includes(item.operation)) graphInvalid('unknown operation');
      graphString(item.namespace, 'namespace'); graphString(item.key, 'key'); graphUnsigned(item.revision, 64, 'revision');
      if (['put', 'delete'].includes(item.operation)) graphUnsigned(item.expectedId, 128, 'expectedId');
      else if (item.expectedId !== undefined) graphInvalid('expectedId is only valid for put/delete');
      if (item.operation === 'recreate') graphUnsigned(item.expectedDeletionRevision, 64, 'expectedDeletionRevision');
      else if (item.expectedDeletionRevision !== undefined) graphInvalid('expectedDeletionRevision is only valid for recreate');
      if (item.detach !== undefined && (item.operation !== 'delete' || item.kind !== 'node' || typeof item.detach !== 'boolean')) graphInvalid('detach is only valid for node delete');
      if (item.operation === 'delete') {
        for (const key of ['labels', 'properties', 'text', 'type', 'source', 'target']) if (item[key] !== undefined) graphInvalid(`delete does not accept ${key}`);
        continue;
      }
      graphProperties(item.properties);
      if (item.kind === 'node') {
        if (item.type !== undefined || item.source !== undefined || item.target !== undefined) graphInvalid('node cannot have relationship fields');
        if (item.labels !== undefined) {
          if (!Array.isArray(item.labels)) graphInvalid('labels must be an array');
          for (const label of item.labels) graphString(label, 'label');
        }
        if (item.text !== undefined) graphString(item.text, 'text');
      } else {
        if (item.labels !== undefined || item.text !== undefined) graphInvalid('relationship cannot have node fields');
        graphString(item.type, 'type'); graphEndpoint(item.source); graphEndpoint(item.target);
      }
    }
    return callNative(() => (async ? binding.graphApplyAsync : binding.graphApply)(this.#native, items));
  }
  cypher(text, params = {}, options = {}) { return this.#cypher(text, params, options, false); }
  async cypherAsync(text, params = {}, options = {}) {
    try {
      return await withSignal(options.signal, token => this.#cypher(text, params, options, true, token ?? 0n));
    } catch (error) { throw translateError(error); }
  }
  #cypher(text, params, options, async, token) {
    graphString(text, 'query'); graphObject(params, null, 'parameters');
    const state = { elements: 0 };
    for (const [name, value] of Object.entries(params)) { graphString(name, 'parameter name'); graphParameter(value, state); }
    graphObject(options, async ? ['maxRows', 'signal'] : ['maxRows'], 'query options');
    const maxRows = options.maxRows ?? 0;
    if (!Number.isInteger(maxRows) || maxRows < 0 || maxRows > 65536) graphInvalid('maxRows must be in 0..65536');
    return callNative(() => async ? binding.graphCypherAsync(this.#native, text, params, maxRows, token) : binding.graphCypher(this.#native, text, params, maxRows));
  }
}

module.exports = {
  GraphStore,
  ABI_VERSION: binding.abiVersion,
  CancellationToken,
  Store,
  UnsupportedPlatformError,
  UnsupportedRuntimeError,
  ZeppelinError,
  idToUuid,
  listNamespaces,
  namespaceBatch,
  namespaceBatchLive,
  declareCascade,
  deleteCascade,
  openNamespace,
  openNamespaceAsync,
  openInspection,
  uuidToId,
  verify,
};
