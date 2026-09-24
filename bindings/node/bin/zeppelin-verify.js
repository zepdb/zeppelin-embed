#!/usr/bin/env node
'use strict';

/**
 * zeppelin-verify <store-directory>
 *
 * Prints the `verify` report for one store directory as JSON on stdout, with
 * 64-bit values as decimal strings. Exits 0 when the store is clean, 1 when it
 * found damage, and 2 when it could not run (usage error, missing path, a path
 * that is not a directory). The store is never opened or modified.
 */

const USAGE = 'usage: zeppelin-verify <store-directory>';

function main(argv) {
  if (argv.length === 1 && (argv[0] === '--help' || argv[0] === '-h')) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }
  if (argv.length !== 1 || argv[0].startsWith('-')) {
    process.stderr.write(`${USAGE}\n`);
    return 2;
  }
  let report;
  try {
    report = require('..').verify(argv[0]);
  } catch (error) {
    const code = typeof error?.code === 'string' ? `${error.code}: ` : '';
    process.stderr.write(`zeppelin-verify: ${code}${error?.message ?? error}\n`);
    return 2;
  }
  const json = JSON.stringify(
    report,
    (_key, value) => (typeof value === 'bigint' ? value.toString() : value),
    2,
  );
  process.stdout.write(`${json}\n`);
  return report.ok ? 0 : 1;
}

process.exitCode = main(process.argv.slice(2));
