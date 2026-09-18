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
    return new ZeppelinError(error.message, error.code, error.errorCode);
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

class Store {
  constructor(storePath, options = {}) {
    this._native = callNative(() => new binding.NativeStore(storePath, options));
  }

  ingest(documents, dimension) {
    return callNative(() => this._native.ingest(documents, dimension));
  }

  upsert(documents) {
    return callNative(() => this._native.upsert(documents));
  }

  get(ids, fields) {
    return callNative(() => this._native.get(ids, fields));
  }

  delete(ids) {
    return callNative(() => this._native.delete(ids));
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
   * select fusion. A `cancelToken` is unwrapped to the native handle so the
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

  close() {
    return callNative(() => this._native.close());
  }
}

function openNamespace(root, name, spec, options = {}) {
  return callNative(() => {
    const store = Object.create(Store.prototype);
    store._native = new binding.NativeStore(root, options, name, spec);
    return store;
  });
}

function listNamespaces(root) {
  return callNative(() => binding.listNamespaces(root));
}

module.exports = {
  ABI_VERSION: binding.abiVersion,
  CancellationToken,
  Store,
  UnsupportedPlatformError,
  UnsupportedRuntimeError,
  ZeppelinError,
  listNamespaces,
  openNamespace,
};
