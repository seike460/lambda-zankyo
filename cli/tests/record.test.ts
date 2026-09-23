import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { CliError } from '../src/errors.ts';
import {
  isKnownFailureType,
  keyMatchesRequestId,
  parseRecord,
  parseRecordKey,
} from '../src/record.ts';

const VALID = {
  version: '1',
  functionName: 'my-api',
  functionVersion: '12',
  requestId: 'req-1',
  invokedAt: '2026-09-22T12:34:56Z',
  failureType: 'handler_error',
  event: { hello: 'world' },
  errorContext: { errorType: 'Error', errorMessage: 'boom' },
  scrubReport: { fieldsRedacted: 0, patternsApplied: [] },
};

describe('parseRecord', () => {
  it('accepts a valid record', () => {
    const rec = parseRecord(JSON.stringify(VALID), 'test');
    assert.equal(rec.requestId, 'req-1');
    assert.equal(rec.failureType, 'handler_error');
  });

  it('rejects malformed JSON', () => {
    assert.throws(() => parseRecord('{oops', 'test'), CliError);
  });

  it('rejects a record missing required fields', () => {
    const { requestId: _dropped, ...rest } = VALID;
    assert.throws(() => parseRecord(JSON.stringify(rest), 'test'), CliError);
  });

  it('accepts unknown failureType for forward compatibility', () => {
    const newer = { ...VALID, failureType: 'exploded' };
    const rec = parseRecord(JSON.stringify(newer), 'test');
    assert.equal(rec.failureType, 'exploded');
    assert.equal(isKnownFailureType(rec.failureType), false);
    assert.equal(isKnownFailureType('handler_error'), true);
  });
});

describe('parseRecordKey', () => {
  it('parses the s3 layout', () => {
    assert.deepEqual(parseRecordKey('zankyo/my-api/2026/09/22/req-1.json'), {
      functionName: 'my-api',
      requestId: 'req-1',
    });
  });

  it('rejects keys outside the layout', () => {
    assert.equal(parseRecordKey('other/req-1.json'), null);
    assert.equal(parseRecordKey('zankyo/req-1.json'), null);
    assert.equal(parseRecordKey('zankyo/fn/2026/09/22/req-1.txt'), null);
  });
});

describe('keyMatchesRequestId', () => {
  it('matches the trailing segment', () => {
    assert.equal(keyMatchesRequestId('zankyo/fn/2026/09/22/req-1.json', 'req-1'), true);
    assert.equal(keyMatchesRequestId('zankyo/fn/2026/09/22/req-1.json', 'req'), false);
  });
});
