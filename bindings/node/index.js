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
      `@zepdb/zeppelin-embed supports macOS on Apple silicon and Windows x64; ` +
        `received ${platform}/${arch}`,
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

  if (platform === 'darwin' && arch === 'arm64') {
    return path.join(prebuilds, 'darwin-arm64', 'zeppelin_embed.node');
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
  Store,
  UnsupportedPlatformError,
  UnsupportedRuntimeError,
  ZeppelinError,
  listNamespaces,
  openNamespace,
};
