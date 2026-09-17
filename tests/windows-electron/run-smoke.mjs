/**
 * Drives the Electron qualification.
 *
 * The consumer installs the package the same way anyone else would: from the
 * tarball `npm pack` produces, not from the source tree. It then runs the
 * fixture twice -- once in development, once as a packaged application -- and
 * requires both to pass.
 *
 * Both runs matter, and the second is the one that has historically failed
 * elsewhere: development resolves modules from `node_modules` on disk, while a
 * packaged application resolves them from inside an ASAR archive, from which a
 * `.node` binary cannot be loaded at all unless it was unpacked. A passing
 * development run proves nothing about the packaged one.
 */
import { spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const fixtureDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(fixtureDirectory, '..', '..');
const nodePackage = join(repository, 'bindings', 'node');

const devOnly = process.argv.includes('--dev-only');
const packagedOnly = process.argv.includes('--packaged-only');

function npmCli() {
  const beside = join(dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js');
  if (existsSync(beside)) return beside;
  const unix = join(
    dirname(dirname(process.execPath)),
    'lib',
    'node_modules',
    'npm',
    'bin',
    'npm-cli.js',
  );
  if (existsSync(unix)) return unix;
  throw new Error(`could not locate npm-cli.js next to ${process.execPath}`);
}
const NPM_CLI = npmCli();

function run(command, args, options = {}) {
  const result = spawnSync(command, args, { stdio: 'inherit', ...options });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} exited with ${result.status}`);
  }
  return result;
}

function runNpm(args, options = {}) {
  // npm is a `.cmd` shim on Windows and current Node will not spawn one
  // without a shell; going through this Node executable avoids the question.
  return run(process.execPath, [NPM_CLI, ...args], options);
}

function capture(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: 'utf8', ...options });
  if (result.error) throw result.error;
  return result;
}

if (process.platform !== 'win32' || process.arch !== 'x64') {
  throw new Error(
    `This qualification targets win32-x64; received ${process.platform}/${process.arch}`,
  );
}

// 1. Pack the package and install that tarball, so the fixture consumes what a
//    user would receive.
const staging = join(fixtureDirectory, '.staging');
rmSync(staging, { force: true, recursive: true });
mkdirSync(staging, { recursive: true });

const packed = capture(process.execPath, [NPM_CLI, 'pack', '--json', '--pack-destination', staging], {
  cwd: nodePackage,
});
if (packed.status !== 0) {
  throw new Error(`npm pack failed:\n${packed.stderr}`);
}
const report = JSON.parse(packed.stdout);
// The fixture's package.json declares this dependency at a fixed relative
// path, so the versioned tarball is renamed to that name.
//
// It must be a *declared production dependency*, not a `--no-save` side-load:
// electron-builder packs production dependencies and treats anything else as
// extraneous. An earlier attempt side-loaded it and produced a 14 KB app.asar
// containing no addon at all, which the unpack check below caught.
const tarball = join(staging, 'zeppelin-embed.tgz');
renameSync(join(staging, report[0].filename), tarball);
console.log(`packed ${report[0].filename} -> .staging/zeppelin-embed.tgz`);

const electronBinary = join(
  fixtureDirectory,
  'node_modules',
  'electron',
  'dist',
  'electron.exe',
);
// One install covers the pinned Electron tooling and the file: dependency
// declared above. `--install-links` packs the tarball's contents into
// node_modules rather than linking, which is what electron-builder needs.
runNpm(['install', '--no-audit', '--no-fund'], { cwd: fixtureDirectory });
if (!existsSync(electronBinary)) {
  // npm's install-script gate can skip Electron's binary download; run the
  // package's own installer explicitly rather than approving scripts wholesale.
  run(process.execPath, [join(fixtureDirectory, 'node_modules', 'electron', 'install.js')], {
    cwd: fixtureDirectory,
  });
}

const evidence = join(repository, 'tasks', 'evidence', 'windows');
mkdirSync(evidence, { recursive: true });

function readReport(file) {
  if (!existsSync(file)) throw new Error(`the fixture wrote no report at ${file}`);
  return JSON.parse(readFileSync(file, 'utf8'));
}

function assertPassed(label, report) {
  console.log(`--- ${label}: ${report.passed} passed, ${report.failed} failed ---`);
  for (const result of report.results) {
    console.log(`  ${result.passed ? 'ok  ' : 'FAIL'} ${result.name}`);
  }
  if (report.failed !== 0) {
    throw new Error(`${label} had ${report.failed} failing scenario(s)`);
  }
}

// 2. Development run.
if (!packagedOnly) {
  const devReport = join(evidence, 'w10-electron-dev.json');
  rmSync(devReport, { force: true });
  run(electronBinary, ['.'], {
    cwd: fixtureDirectory,
    env: { ...process.env, ZE_FIXTURE_REPORT: devReport },
  });
  const parsed = readReport(devReport);
  if (parsed.packaged !== false) {
    throw new Error('the development run reported itself as packaged');
  }
  assertPassed('development', parsed);
}

// 3. Packaged run.
if (!devOnly) {
  // Each run gets its own output directory.
  //
  // Windows keeps a freshly written file open for a while after the writer is
  // gone -- an indexer or an anti-malware scanner will hold `app.asar` long
  // enough that deleting a previous `dist` fails with EPERM even though no
  // process of ours is alive. Writing somewhere new sidesteps that entirely
  // instead of retrying against a lock nothing here controls. Stale
  // directories are swept on a best-effort basis.
  const outputDirectory = join(fixtureDirectory, 'dist', `run-${process.pid}`);
  const distRoot = join(fixtureDirectory, 'dist');
  if (existsSync(distRoot)) {
    for (const entry of readdirSync(distRoot)) {
      try {
        rmSync(join(distRoot, entry), { force: true, recursive: true });
      } catch {
        // A locked leftover is not this run's problem.
      }
    }
  }
  mkdirSync(outputDirectory, { recursive: true });

  run(
    process.execPath,
    [
      join(fixtureDirectory, 'node_modules', 'electron-builder', 'cli.js'),
      '--win',
      '--x64',
      '--dir',
      `--config.directories.output=${outputDirectory}`,
    ],
    {
      cwd: fixtureDirectory,
      // This fixture is deliberately unsigned; automatic signing discovery
      // would otherwise fail or, worse, pick up an unrelated certificate.
      env: { ...process.env, CSC_IDENTITY_AUTO_DISCOVERY: 'false' },
    },
  );

  const packagedExe = join(outputDirectory, 'win-unpacked', 'ZeppelinWindowsFixture.exe');
  if (!existsSync(packagedExe)) {
    throw new Error(`electron-builder produced no executable at ${packagedExe}`);
  }

  // The addon must have been unpacked out of the ASAR archive. A `.node` file
  // left inside `app.asar` cannot be loaded at all, so this is checked before
  // the app runs rather than inferred from a failure.
  const unpacked = join(
    outputDirectory,
    'win-unpacked',
    'resources',
    'app.asar.unpacked',
    'node_modules',
    '@zepdb',
    'zeppelin-embed',
    'prebuilds',
    'win32-x64',
    'electron-44',
    'zeppelin_embed.node',
  );
  if (!existsSync(unpacked)) {
    throw new Error(
      `the Electron addon was not unpacked from the ASAR archive; expected ${unpacked}`,
    );
  }
  console.log('addon unpacked from app.asar: ok');

  const packagedReport = join(evidence, 'w10-electron-packaged.json');
  rmSync(packagedReport, { force: true });
  // A deliberately minimal environment: no repository PATH entries, so the
  // packaged application cannot quietly pick up a DLL from the build tree.
  const cleanEnv = {
    SystemRoot: process.env.SystemRoot,
    windir: process.env.windir,
    TEMP: process.env.TEMP,
    TMP: process.env.TMP,
    USERPROFILE: process.env.USERPROFILE,
    LOCALAPPDATA: process.env.LOCALAPPDATA,
    APPDATA: process.env.APPDATA,
    NUMBER_OF_PROCESSORS: process.env.NUMBER_OF_PROCESSORS,
    PATH: `${process.env.SystemRoot}\\system32;${process.env.SystemRoot}`,
    ZE_FIXTURE_REPORT: packagedReport,
  };
  run(packagedExe, [], { cwd: dirname(packagedExe), env: cleanEnv });
  const parsed = readReport(packagedReport);
  if (parsed.packaged !== true) {
    throw new Error('the packaged run did not report itself as packaged');
  }
  assertPassed('packaged', parsed);
}

// `.staging/zeppelin-embed.tgz` is referenced by package.json, so it stays.
console.log('ELECTRON QUALIFICATION PASSED');
