import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const BIN = fileURLToPath(new URL('../src/bin.ts', import.meta.url));

function run(...args: string[]): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolvePromise) => {
    execFile(process.execPath, [BIN, ...args], (error, stdout, stderr) => {
      resolvePromise({
        // ExecFileException.code は number か errno 文字列。数値だけを採用
        code: error && typeof error.code === 'number' ? error.code : error ? 1 : 0,
        stdout,
        stderr,
      });
    });
  });
}

describe('zankyo bin (e2e)', () => {
  it('prints usage on --help with exit 0', async () => {
    const r = await run('--help');
    assert.equal(r.code, 0);
    assert.ok(r.stdout.includes('commands:'));
    assert.ok(r.stdout.includes('redrive'));
  });

  it('prints usage when no command is given', async () => {
    const r = await run();
    assert.equal(r.code, 0);
    assert.ok(r.stdout.includes('usage:'));
  });

  it('rejects unknown commands with exit 2', async () => {
    const r = await run('frobnicate');
    assert.equal(r.code, 2);
    assert.ok(r.stderr.includes('unknown command'));
  });

  it('reports --version', async () => {
    const r = await run('--version');
    assert.equal(r.code, 0);
    assert.match(r.stdout.trim(), /^\d+\.\d+\.\d+$/);
  });
});
