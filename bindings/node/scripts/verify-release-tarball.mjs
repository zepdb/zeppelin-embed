/**
 * Verifies that a packed tarball actually contains every binary the package
 * metadata advertises.
 *
 * `npm pack` silently omits entries listed in `files` that do not exist on
 * disk. One machine cannot build both the macOS and the Windows binaries, so
 * packing on either platform alone produces a tarball that claims support for
 * both and ships only one. Installing that on the other platform fails at
 * `require` time, after the install appeared to succeed.
 *
 * This is therefore the release-assembly gate, deliberately separate from
 * `check:prebuilt`. `check:prebuilt` guards a *local* pack and only requires
 * what the running platform can build; this requires every advertised cell and
 * is what a release must pass.
 *
 * Usage:
 *   node scripts/verify-release-tarball.mjs <tarball>
 */
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const packageDirectory = resolve(scriptDirectory, '..');

const tarball = process.argv[2];
if (!tarball) {
  console.error('usage: node scripts/verify-release-tarball.mjs <tarball>');
  process.exit(2);
}

const manifest = JSON.parse(readFileSync(resolve(packageDirectory, 'package.json'), 'utf8'));

/**
 * Every binary path the metadata advertises.
 *
 * Taken from `files` rather than hard-coded, so adding a platform to the
 * package cannot silently escape this gate.
 */
const advertised = manifest.files.filter((entry) => entry.endsWith('.node'));
if (advertised.length === 0) {
  throw new Error('package.json `files` advertises no native binaries; nothing to verify');
}

// `tar -tf` lists the archive without extracting it. bsdtar ships with Windows.
const listed = spawnSync('tar', ['-tf', tarball], { encoding: 'utf8' });
if (listed.error) throw listed.error;
if (listed.status !== 0) {
  throw new Error(`tar -tf ${tarball} exited with ${listed.status}\n${listed.stderr}`);
}

// npm tarballs root every entry under `package/`.
const entries = new Set(
  listed.stdout
    .split('\n')
    .map((line) => line.trim().replace(/^package\//, ''))
    .filter(Boolean),
);

const missing = advertised.filter((binary) => !entries.has(binary));

console.error(`tarball: ${tarball}`);
console.error(`advertised binaries: ${advertised.length}`);
console.error(`present: ${advertised.length - missing.length}`);

if (missing.length > 0) {
  console.error('');
  console.error('This tarball advertises binaries it does not contain:');
  for (const binary of missing) console.error(`  MISSING  ${binary}`);
  console.error('');
  console.error(
    `It also declares os=${JSON.stringify(manifest.os)} cpu=${JSON.stringify(manifest.cpu)}, ` +
      'so npm would install it on a platform whose binary is absent and the failure would ' +
      'surface at require time.',
  );
  console.error(
    'Build the missing cells on their own platforms and combine the artifacts before release.',
  );
  process.exit(1);
}

console.error('RELEASE TARBALL COMPLETE: every advertised binary is present');
