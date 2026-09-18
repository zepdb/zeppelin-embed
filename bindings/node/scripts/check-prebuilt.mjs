import { access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

/**
 * Prebuilt binaries this package advertises, by the platform that can produce
 * them.
 *
 * `npm pack` must never produce a tarball that claims a binary it does not
 * contain, so this refuses to pack unless every binary buildable on the
 * running platform is present. It deliberately does not require the *other*
 * platform's binary: one machine cannot build both, and the release assembly
 * step is what combines them. That boundary is stated here rather than
 * silently assumed.
 */
const REQUIRED = {
  darwin: [
    '../prebuilds/darwin-arm64/zeppelin_embed.node',
    '../prebuilds/darwin-x64/zeppelin_embed.node',
  ],
  win32: [
    '../prebuilds/win32-x64/node-napi8/zeppelin_embed.node',
    '../prebuilds/win32-x64/electron-44/zeppelin_embed.node',
  ],
};

/**
 * Architectures that can *build* each platform's binaries.
 *
 * Both macOS slices cross-compile, so either kind of Mac produces the whole
 * macOS set and `REQUIRED.darwin` lists both. Windows builds only its own.
 */
const BUILD_ARCH = { darwin: ['arm64', 'x64'], win32: ['x64'] };

const buildArch = BUILD_ARCH[process.platform];
if (buildArch === undefined) {
  throw new Error(
    `Native packages can be built on macOS and Windows x64; received ${process.platform}/${process.arch}`,
  );
}
if (!buildArch.includes(process.arch)) {
  throw new Error(
    `Native packages for ${process.platform} are built on ${buildArch.join(' or ')}; ` +
      `received ${process.arch}`,
  );
}

const missing = [];
for (const relative of REQUIRED[process.platform]) {
  const binary = fileURLToPath(new URL(relative, import.meta.url));
  try {
    await access(binary);
  } catch {
    missing.push(binary);
  }
}

if (missing.length > 0) {
  throw new Error(
    `Missing native addon(s):\n  ${missing.join('\n  ')}\n` +
      'Run `npm run build:native` first. The Electron variant additionally ' +
      'needs ZE_ELECTRON_VERSION set, for example ' +
      'ZE_ELECTRON_VERSION=44.4.1 npm run build:native.',
  );
}

// Deliberately stderr. This runs as npm's `prepack` lifecycle script, and
// `npm pack --json` emits machine-readable JSON on stdout; anything written
// there by a lifecycle script corrupts it for the caller parsing it.
console.error(
  `prebuilt binaries present for ${process.platform}-${process.arch}: ` +
    `${REQUIRED[process.platform].length}`,
);
