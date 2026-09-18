import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

for (const mode of ['same-process', 'worker']) {
  test(`native double load and finalization: ${mode}`, () => {
    const root = mkdtempSync(join(tmpdir(), 'ze-double-load-'));
    try {
      const result = spawnSync(
        process.execPath,
        [
          '--expose-gc',
          fileURLToPath(new URL('./double-load-child.cjs', import.meta.url)),
          mode,
          root,
        ],
        { encoding: 'utf8', timeout: 30000 },
      );
      assert.ifError(result.error);
      assert.equal(result.signal, null, result.stderr);
      assert.equal(result.status, 0, result.stderr);
      assert.match(result.stdout, new RegExp(`WF-137 ${mode} passed`));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
}
