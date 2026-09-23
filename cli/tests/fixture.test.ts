import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { CliError } from '../src/errors.ts';
import { buildFixtureEvent, eventPayload, fixtureJson } from '../src/fixture.ts';
import type { ZankyoRecord } from '../src/record.ts';

const base: ZankyoRecord = {
  version: '1',
  functionName: 'fn',
  functionVersion: '1',
  requestId: 'r1',
  invokedAt: '2026-09-22T00:00:00Z',
  failureType: 'handler_error',
  event: { input: 'data' },
  errorContext: {},
};

describe('buildFixtureEvent', () => {
  it('returns the stored event', () => {
    assert.deepEqual(buildFixtureEvent(base), { input: 'data' });
  });

  it('rejects truncated records', () => {
    assert.throws(() => buildFixtureEvent({ ...base, truncated: true }), CliError);
  });

  it('rejects records without events (init_error)', () => {
    const initRec: ZankyoRecord = { ...base, failureType: 'init_error', event: null };
    assert.throws(() => buildFixtureEvent(initRec), CliError);
  });
});

describe('fixtureJson', () => {
  it('emits pretty JSON with trailing newline', () => {
    const out = fixtureJson(base);
    assert.ok(out.endsWith('\n'));
    assert.deepEqual(JSON.parse(out), { input: 'data' });
  });

  it('writes raw text events verbatim (no JSON quoting)', () => {
    const raw: ZankyoRecord = { ...base, event: '<xml>not json</xml>', eventIsRawText: true };
    assert.equal(fixtureJson(raw), '<xml>not json</xml>\n');
  });
});

describe('eventPayload', () => {
  it('JSON-encodes normal events', () => {
    assert.equal(eventPayload(base), '{"input":"data"}');
  });

  it('passes raw text through unchanged', () => {
    const raw: ZankyoRecord = { ...base, event: 'hello, world', eventIsRawText: true };
    assert.equal(eventPayload(raw), 'hello, world');
  });

  it('still rejects truncated records', () => {
    assert.throws(() => eventPayload({ ...base, truncated: true }), CliError);
  });
});
