import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import { run as runDiff } from '../src/commands/diff.ts';
import { run as runFixture } from '../src/commands/fixture.ts';
import { run as runList } from '../src/commands/list.ts';
import { run as runRedrive } from '../src/commands/redrive.ts';
import { run as runReplay } from '../src/commands/replay.ts';
import { CliError } from '../src/errors.ts';
import {
  captureStdout,
  deps,
  fakeLambda,
  fakeLambdaHandler,
  fakeLambdaSeq,
  fakeS3,
  rejecting,
  sentPayload,
  VALID_RECORD,
} from './helpers.ts';

const KEY = 'zankyo/fn/2026/09/22/r1.json';
const RECORDED_EVENT = { user: 'alice', password: 'x***' };

const isExit = (code: number) => (e: unknown) => e instanceof CliError && e.exitCode === code;

const RAW_TEXT_RECORD = JSON.stringify({
  ...(JSON.parse(VALID_RECORD) as Record<string, unknown>),
  event: '<xml>not json</xml>',
  eventIsRawText: true,
});

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
    const parsed: unknown = JSON.parse(out);
    assert(Array.isArray(parsed) && parsed.length === 1);
    const first = parsed[0];
    assert(typeof first === 'object' && first !== null && 'key' in first);
    assert.equal(first.key, KEY);
  });

  it('exits 3 when S3 rejects the listing', async () => {
    const s3 = { ...fakeS3([]), listObjectsV2: rejecting('AccessDenied') };
    await assert.rejects(() => runList(['--bucket', 'b'], deps(s3)), isExit(3));
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
    const written: unknown = JSON.parse(await readFile(outFile, 'utf8'));
    assert(typeof written === 'object' && written !== null && 'user' in written);
    assert.equal(written.user, 'alice');
  });

  it('creates --out with mode 0600', { skip: process.platform === 'win32' }, async (t) => {
    const dir = await mkdtemp(join(tmpdir(), 'zankyo-'));
    t.after(() => rm(dir, { recursive: true, force: true }));
    const outFile = join(dir, 'event.json');
    const s3 = fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);
    await captureStdout(() => runFixture(['r1', '--bucket', 'b', '--out', outFile], deps(s3)));
    assert.equal((await stat(outFile)).mode & 0o777, 0o600);
  });

  it('rejects when no requestId or --last given', async () => {
    await assert.rejects(() => runFixture(['--bucket', 'b'], deps(fakeS3([]))), CliError);
  });

  it('exits 3 when S3 rejects the read', async () => {
    const s3 = { ...fakeS3([{ Contents: [{ Key: KEY }] }]), getObject: rejecting('AccessDenied') };
    await assert.rejects(() => runFixture(['r1', '--bucket', 'b'], deps(s3)), isExit(3));
  });
});

describe('zankyo replay', () => {
  const s3 = () => fakeS3([{ Contents: [{ Key: KEY }] }], VALID_RECORD);

  it('invokes the recorded event and prints the outcome', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      Payload: new TextEncoder().encode('{"ok":true}'),
    });
    const { code, out } = await captureStdout(() =>
      runReplay(['r1', '--bucket', 'b', '--alias', 'dev'], deps(s3(), lambda)),
    );
    assert.equal(code, 0);
    assert.ok(out.includes('fn:dev'));
    assert.ok(out.includes('{"ok":true}'));
    assert.equal(lambda.inputs.length, 1);
    assert.equal(lambda.inputs[0]?.FunctionName, 'fn:dev');
    assert.deepEqual(sentPayload(lambda.inputs[0]), RECORDED_EVENT);
  });

  it('exits 1 when the function returns an error', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      FunctionError: 'Unhandled',
      Payload: new TextEncoder().encode('{"errorMessage":"x"}'),
    });
    const { code } = await captureStdout(() =>
      runReplay(['r1', '--bucket', 'b'], deps(s3(), lambda)),
    );
    assert.equal(code, 1);
  });

  it('exits 3 when the Invoke API call fails', async () => {
    const lambda = { invoke: rejecting('AccessDeniedException') };
    await assert.rejects(() => runReplay(['r1', '--bucket', 'b'], deps(s3(), lambda)), isExit(3));
  });

  it('exits 4 without invoking when the record holds a non-JSON event', async () => {
    const lambda = fakeLambda({ StatusCode: 200 });
    await assert.rejects(
      () =>
        runReplay(
          ['r1', '--bucket', 'b'],
          deps(fakeS3([{ Contents: [{ Key: KEY }] }], RAW_TEXT_RECORD), lambda),
        ),
      isExit(4),
    );
    assert.deepEqual(lambda.inputs, []);
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
    assert.deepEqual(lambda.inputs.map((i) => i.FunctionName).sort(), ['fn:a', 'fn:b']);
    assert.deepEqual(lambda.inputs.map(sentPayload), [RECORDED_EVENT, RECORDED_EVENT]);
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
    assert.deepEqual(lambda.inputs, []);
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
    assert.equal(lambda.inputs.length, 1);
    assert.equal(lambda.inputs[0]?.FunctionName, 'fn');
    assert.deepEqual(sentPayload(lambda.inputs[0]), RECORDED_EVENT);
  });

  it('refuses to invoke a function other than the one in the record key', async () => {
    const forged = JSON.stringify({
      ...(JSON.parse(VALID_RECORD) as Record<string, unknown>),
      functionName: 'other-fn',
    });
    const invoked: string[] = [];
    const lambda = fakeLambdaHandler((input) => {
      invoked.push(input.FunctionName);
      return { StatusCode: 200 };
    });
    await assert.rejects(
      () =>
        runRedrive(
          ['r1', '--bucket', 'b', '--confirm'],
          deps(fakeS3([{ Contents: [{ Key: KEY }] }], forged), lambda),
        ),
      (e: unknown) => e instanceof CliError && e.exitCode === 4,
    );
    assert.deepEqual(invoked, []);
  });
});
