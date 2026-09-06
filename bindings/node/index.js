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
      `@zepdb/zeppelin-embed supports macOS on Apple silicon; received ${platform}/${arch}`,
    );
    this.name = 'UnsupportedPlatformError';
    this.code = 'ERR_ZEPPELIN_UNSUPPORTED_PLATFORM';
  }
}

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  throw new UnsupportedPlatformError(process.platform, process.arch);
}

const binding = require(
  path.join(__dirname, 'prebuilds', 'darwin-arm64', 'zeppelin_embed.node'),
);

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
  ZeppelinError,
  listNamespaces,
  openNamespace,
};
