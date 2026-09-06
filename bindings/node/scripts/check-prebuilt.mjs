import { access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'darwin' || process.arch !== 'arm64') {
  throw new Error(
    `Native packages can only be built on macOS arm64; received ${process.platform}/${process.arch}`,
  );
}

const binary = fileURLToPath(
  new URL('../prebuilds/darwin-arm64/zeppelin_embed.node', import.meta.url),
);

try {
  await access(binary);
} catch {
  throw new Error(`Missing native addon at ${binary}; run npm run build:native first`);
}
