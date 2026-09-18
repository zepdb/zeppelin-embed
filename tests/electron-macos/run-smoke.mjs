import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build, Platform, Arch } from 'electron-builder';

const fixture = dirname(fileURLToPath(import.meta.url));
const repository = resolve(fixture, '../..');
const arch = process.argv[2];
const mode = process.argv[3];
assert.equal(process.platform, 'darwin');
assert.ok(['arm64', 'x64'].includes(arch), 'pass arm64 or x64');
assert.ok(['single', 'concurrent'].includes(mode), 'pass single or concurrent');
const stage = join(fixture, '.staging');
const appSource = join(stage, 'app');
const output = join(fixture, 'dist', arch);
const evidence = join(fixture, 'evidence', `${arch}-${mode}`);
rmSync(stage, { recursive: true, force: true });
rmSync(output, { recursive: true, force: true });
rmSync(evidence, { recursive: true, force: true });
mkdirSync(appSource, { recursive: true });
mkdirSync(evidence, { recursive: true });

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    timeout: 120000,
    ...options,
  });
  if (result.error) throw result.error;
  assert.equal(result.signal, null, `${command}: ${result.stderr}`);
  assert.equal(
    result.status,
    0,
    `${command}: ${result.stdout}\n${result.stderr}`,
  );
  return result;
}
const npm = process.env.npm_execpath;
assert.ok(npm, 'invoke through npm run smoke -- <arch> <mode>');
const packed = JSON.parse(
  run(process.execPath, [npm, 'pack', '--json', '--pack-destination', stage], {
    cwd: join(repository, 'bindings/node'),
  }).stdout,
)[0];
const tarball = join(stage, packed.filename);
writeFileSync(
  join(evidence, 'tarball.sha256'),
  createHash('sha256').update(readFileSync(tarball)).digest('hex') + '\n',
);
writeFileSync(
  join(appSource, 'package.json'),
  JSON.stringify(
    {
      name: 'zeppelin-electron-consumer',
      version: '0.0.0',
      private: true,
      description: 'ADR-012 packaged lifecycle probe',
      license: 'GPL-3.0-only',
      main: 'main.cjs',
      dependencies: { '@zepdb/zeppelin-embed': `file:${tarball}` },
    },
    null,
    2,
  ) + '\n',
);
for (const file of ['main.cjs', 'utility.cjs'])
  copyFileSync(join(fixture, file), join(appSource, file));
copyFileSync(
  join(repository, 'bindings/node/test/double-load.cjs'),
  join(appSource, 'double-load.cjs'),
);
run(
  process.execPath,
  [
    npm,
    'install',
    '--ignore-scripts',
    '--no-package-lock',
    '--no-audit',
    '--no-fund',
  ],
  { cwd: appSource },
);
// Use the API: electron-builder's CLI auto-loads an env file. This fixture does not.
process.env.CSC_IDENTITY_AUTO_DISCOVERY = 'false';
await build({
  projectDir: fixture,
  targets: Platform.MAC.createTarget(['dir'], Arch[arch]),
  config: {
    appId: 'dev.zepdb.electron.fixture',
    productName: 'ZeppelinElectronFixture',
    electronVersion: '44.4.1',
    npmRebuild: false,
    asar: true,
    asarUnpack: ['node_modules/@zepdb/zeppelin-embed/prebuilds/**/*.node'],
    files: ['*.cjs', 'package.json'],
    directories: { app: appSource, output },
    mac: {
      identity: '-',
      hardenedRuntime: true,
      notarize: false,
      entitlements: join(fixture, 'entitlements.plist'),
      entitlementsInherit: join(fixture, 'entitlements.plist'),
      gatekeeperAssess: false,
      strictVerify: true,
    },
  },
});
const app = join(
  output,
  arch === 'arm64' ? 'mac-arm64' : 'mac',
  'ZeppelinElectronFixture.app',
);
const contents = join(app, 'Contents');
const binary = join(contents, 'MacOS/ZeppelinElectronFixture');
const helper = join(contents, 'Frameworks/ZeppelinElectronFixture Helper.app');
const addon = join(
  contents,
  'Resources/app.asar.unpacked/node_modules/@zepdb/zeppelin-embed/prebuilds',
  `darwin-${arch}`,
  'zeppelin_embed.node',
);
assert.ok(existsSync(join(contents, 'Resources/app.asar')));
assert.ok(existsSync(addon), 'native addon must be unpacked');
const signatures = [];
for (const file of [app, helper, addon]) {
  const verified = run('codesign', [
    '--verify',
    '--strict',
    '--verbose=2',
    file,
  ]);
  const details = run('codesign', ['--display', '--verbose=4', file]).stderr;
  assert.match(details, /Signature=adhoc/);
  assert.match(details, /flags=.*\bruntime\b/);
  signatures.push({ file, details, verification: verified.stderr });
}
run('codesign', ['--verify', '--deep', '--strict', app]);
for (const file of [app, helper]) {
  const xml = run('codesign', [
    '--display',
    '--entitlements',
    ':-',
    file,
  ]).stdout;
  const plist = JSON.parse(
    run('plutil', ['-convert', 'json', '-o', '-', '-'], { input: xml }).stdout,
  );
  assert.deepEqual(plist, {
    'com.apple.security.cs.allow-jit': true,
    'com.apple.security.cs.disable-library-validation': true,
  });
}
writeFileSync(
  join(evidence, 'signatures.json'),
  JSON.stringify(signatures, null, 2) + '\n',
);
const root = mkdtempSync(join(tmpdir(), 'ze-electron-packaged-'));
const reportPath = join(evidence, 'report.json');
try {
  // No NODE_PATH, NODE_OPTIONS, development cwd, or ELECTRON_RUN_AS_NODE.
  const env = {
    HOME: process.env.HOME,
    TMPDIR: process.env.TMPDIR || tmpdir(),
    PATH: '/usr/bin:/bin:/usr/sbin:/sbin',
    ZE_FIXTURE_ROOT: root,
    ZE_FIXTURE_REPORT: reportPath,
    ZE_FIXTURE_MODE: mode,
  };
  const launched = spawnSync(
    '/usr/bin/arch',
    [arch === 'x64' ? '-x86_64' : '-arm64', binary],
    {
      cwd: root,
      env,
      encoding: 'utf8',
      timeout: 60000,
    },
  );
  writeFileSync(join(evidence, 'stdout.log'), launched.stdout || '');
  writeFileSync(join(evidence, 'stderr.log'), launched.stderr || '');
  if (launched.error) throw launched.error;
  assert.equal(launched.signal, null, launched.stderr);
  assert.equal(launched.status, 0, launched.stderr);
  const report = JSON.parse(readFileSync(reportPath, 'utf8'));
  assert.equal(report.packaged, true);
  assert.equal(report.electron, '44.4.1');
  assert.equal(report.arch, arch);
  assert.equal(report.mode, mode);
  assert.equal(report.utilities.length, mode === 'concurrent' ? 2 : 1);
  for (const result of [report.main, ...report.utilities]) {
    assert.equal(result.arch, arch);
    assert.equal(result.nativeLoads, 2);
    assert.ok(result.addon.startsWith(join(contents, 'Resources/app.asar')));
    assert.ok(
      result.addon.endsWith(`prebuilds/darwin-${arch}/zeppelin_embed.node`),
    );
  }
  console.log(`WF-137 packaged ${arch} ${mode} passed`);
} finally {
  rmSync(root, { recursive: true, force: true });
}
