import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import { run as runDiff } from '../src/commands/diff.ts';
import { run as runFixture } from '../src/commands/fixture.ts';
import { run as runList } from '../src/commands/list.ts';
import { run as runRedrive } from '../src/commands/redrive.ts';
import { run as runReplay } from '../src/commands/replay.ts';
import { CliError } from '../src/errors.ts';
import { captureStdout, deps, fakeLambda, fakeLambdaSeq, fakeS3, VALID_RECORD } from './helpers.ts';

const KEY = 'zankyo/fn/2026/09/22/r1.json';

describe('zankyo list', () => {
  const s3 = () =>
    fakeS3([{ Contents: [{ Key: KEY, LastModified: new Date('2026-09-22T00:00:00Z') }] }]);

  it('prints a table of records', async () => {
    const { code, out } = await captureStdout(() => runList(['--bucket', 'b'], deps(s3())));
    assert.equal(code, 0);
    assert.ok(out.includes('REQUEST_ID'));
    assert.ok(out.includes('r1'));
    assert.ok(out.includes('fn'));
  });

  it('emits JSON with --json', async () => {
    const { code, out } = await captureStdout(() =>
      runList(['--bucket', 'b', '--json'], deps(s3())),
    );
    assert.equal(code, 0);
    const parsed = JSON.parse(out) as { key: string }[];
    assert.equal(parsed[0]?.key, KEY);
  });
});

describe('zankyo fixture', () => {
  it('writes the stored event to --out', async (t) => {
    const dir = await mkdtemp(join(tmpdir(), 'zankyo-'));
    t.after(() => rm(dir, { recursive: true, force: true }));
    const outFile = join(dir, 'event.json');
    const s3 = fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);
    const { code } = await captureStdout(() =>
      runFixture(['r1', '--bucket', 'b', '--out', outFile], deps(s3)),
    );
    assert.equal(code, 0);
    const written = JSON.parse(await readFile(outFile, 'utf8')) as Record<string, unknown>;
    assert.equal(written.user, 'alice');
  });

  it('rejects when no requestId or --last given', async () => {
    await assert.rejects(() => runFixture(['--bucket', 'b'], deps(fakeS3([]))), CliError);
  });
});

describe('zankyo replay', () => {
  it('invokes the recorded event and prints the outcome', async () => {
    const s3 = fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);
    const lambda = fakeLambda({
      StatusCode: 200,
      Payload: new TextEncoder().encode('{"ok":true}'),
    });
    const { code, out } = await captureStdout(() =>
      runReplay(['r1', '--bucket', 'b', '--alias', 'dev'], deps(s3, lambda)),
    );
    assert.equal(code, 0);
    assert.ok(out.includes('fn:dev'));
    assert.ok(out.includes('{"ok":true}'));
  });
});

describe('zankyo diff', () => {
  const s3 = () => fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);

  it('exits 0 when both aliases agree', async () => {
    const lambda = fakeLambdaSeq([
      { StatusCode: 200, Payload: new TextEncoder().encode('{"v":1}') },
      { StatusCode: 200, Payload: new TextEncoder().encode('{"v":1}') },
    ]);
    const { code } = await captureStdout(() =>
      runDiff(['r1', '--bucket', 'b', '--alias', 'a', '--alias', 'b'], deps(s3(), lambda)),
    );
    assert.equal(code, 0);
  });

  it('exits 1 when responses differ', async () => {
    const lambda = fakeLambdaSeq([
      { StatusCode: 200, Payload: new TextEncoder().encode('{"v":1}') },
      { StatusCode: 200, Payload: new TextEncoder().encode('{"v":2}') },
    ]);
    const { code, out } = await captureStdout(() =>
      runDiff(['r1', '--bucket', 'b', '--alias', 'a', '--alias', 'b'], deps(s3(), lambda)),
    );
    assert.equal(code, 1);
    assert.ok(out.includes('$.v'));
  });
});

describe('zankyo redrive', () => {
  const s3 = () => fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);

  it('defaults to dry-run without invoking', async () => {
    const lambda = fakeLambda({ StatusCode: 200 });
    const { code, out } = await captureStdout(() =>
      runRedrive(['r1', '--bucket', 'b'], deps(s3(), lambda)),
    );
    assert.equal(code, 0);
    assert.ok(out.includes('dry-run'));
  });

  it('invokes with --confirm and reports function errors', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      FunctionError: 'Handled',
      Payload: new TextEncoder().encode('{"errorMessage":"x"}'),
    });
    const { code } = await captureStdout(() =>
      runRedrive(['r1', '--bucket', 'b', '--confirm'], deps(s3(), lambda)),
    );
    assert.equal(code, 1);
  });
});
