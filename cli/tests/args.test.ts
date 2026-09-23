import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { parseSince, resolveBucket } from '../src/args.ts';
import { CliError } from '../src/errors.ts';

describe('parseSince', () => {
  it('parses all units', () => {
    assert.equal(parseSince('30s'), 30_000);
    assert.equal(parseSince('15m'), 900_000);
    assert.equal(parseSince('24h'), 86_400_000);
    assert.equal(parseSince('7d'), 604_800_000);
    assert.equal(parseSince('2w'), 1_209_600_000);
  });

  it('rejects invalid input', () => {
    assert.throws(() => parseSince('yesterday'), CliError);
    assert.throws(() => parseSince('10'), CliError);
    assert.throws(() => parseSince('-5h'), CliError);
    assert.throws(() => parseSince(''), CliError);
  });
});

describe('resolveBucket', () => {
  it('prefers the flag over env', () => {
    process.env.ZANKYO_BUCKET = 'env-bucket';
    assert.equal(resolveBucket({ bucket: 'flag-bucket' }), 'flag-bucket');
  });

  it('falls back to env', () => {
    process.env.ZANKYO_BUCKET = 'env-bucket';
    assert.equal(resolveBucket({}), 'env-bucket');
  });

  it('errors when neither is set', () => {
    delete process.env.ZANKYO_BUCKET;
    assert.throws(() => resolveBucket({}), CliError);
  });
});
