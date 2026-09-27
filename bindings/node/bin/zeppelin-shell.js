#!/usr/bin/env node
'use strict';

const { join } = require('node:path');
const USAGE = `usage: zeppelin-shell <path> <command> [argument]
  namespaces              list namespaces and attribute schemas under root
  schema                  list a store's attribute schema
  get <decimal-id>         get one document with all stored fields
  scan [request-json]     one page; accepts filter, order, limit, fields
  count [request-json]    count, optionally filtered
  query <request-json>    text query, e.g. '{"text":"hello","k":10}'
  dump [request-json]     all matching documents as JSON lines; limit is page size
  verify                  verify without opening; exit 1 on corruption
Always read-only. Use a quiescent copy of the store. Request JSON uses
{"$bigint":"123"} for bigint values. Output integers are decimal strings;
vectors and metadata are arrays; non-finite numbers are named strings.
Exit 0 on success, 1 on verify findings, 2 on usage or engine errors.`;
const fields = { vector: true, text: true, metadata: true, attributes: true };
function request(text = '{}') {
  const value = JSON.parse(text, (_key, value) => {
    if (value && typeof value === 'object' && Object.hasOwn(value, '$bigint')) {
      if (Object.keys(value).length !== 1 || typeof value.$bigint !== 'string' || !/^-?\d+$/.test(value.$bigint)) {
        throw new TypeError('bigint must be {"$bigint":"decimal integer"}');
      }
      return BigInt(value.$bigint);
    }
    return value;
  });
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new TypeError('request must be an object');
  return value;
}
function print(value) {
  process.stdout.write(JSON.stringify(value, (_key, value) => {
    if (typeof value === 'bigint') return value.toString();
    if (ArrayBuffer.isView(value)) return Array.from(value);
    if (typeof value === 'number' && !Number.isFinite(value)) return String(value);
    return value;
  }) + '\n');
}
function main(args) {
  if (args.length === 1 && ['--help', '-h'].includes(args[0])) { console.log(USAGE); return 0; }
  const [path, command, argument] = args;
  const commands = ['namespaces', 'schema', 'get', 'scan', 'count', 'query', 'dump', 'verify'];
  if (args.length < 2 || args.length > 3 || !commands.includes(command) ||
      (['namespaces', 'schema', 'verify'].includes(command) && argument !== undefined) ||
      (['get', 'query'].includes(command) && argument === undefined)) throw new Error(USAGE);
  const { openInspection, listNamespaces, verify } = require('..');
  if (command === 'verify') { const result = verify(path); print(result); return result.ok ? 0 : 1; }
  if (command === 'namespaces') {
    print(listNamespaces(path).map(name => {
      const store = openInspection(join(path, name));
      try { return { name, attributes: store.schema() }; } finally { store.close(); }
    }));
    return 0;
  }
  const store = openInspection(path);
  try {
    switch (command) {
      case 'schema': print(store.schema()); break;
      case 'get':
        if (!/^\d+$/.test(argument)) throw new TypeError('id must be an unsigned decimal integer');
        print(store.get([BigInt(argument)], fields)); break;
      case 'scan': print(store.scan(request(argument))); break;
      case 'count': print(store.count(request(argument))); break;
      case 'query': {
        const options = request(argument);
        if (typeof options.text !== 'string') throw new TypeError('query requires text');
        print(store.query(options)); break;
      }
      case 'dump': {
        const options = request(argument);
        let cursor;
        do {
          const page = store.scan({ ...options, fields, cursor });
          for (const document of page.documents) print(document);
          cursor = page.cursor;
        } while (cursor !== null);
        break;
      }
    }
    return 0;
  } finally { store.close(); }
}
try { process.exitCode = main(process.argv.slice(2)); }
catch (error) {
  console.error(`zeppelin-shell: ${error.code ? error.code + ': ' : ''}${error.message}`);
  process.exitCode = 2;
}
