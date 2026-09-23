import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { CliError } from '../src/errors.ts';
import { fetchRecord, listRecordKeys, resolveRecordKey } from '../src/store.ts';
import { fakeS3 } from './helpers.ts';

function obj(key: string, lastModified?: string) {
  return { Key: key, LastModified: lastModified ? new Date(lastModified) : undefined };
}

const VALID_RECORD = JSON.stringify({
  version: '1',
  functionName: 'fn',
  functionVersion: '1',
  requestId: 'r1',
  invokedAt: '2026-09-22T00:00:00Z',
  failureType: 'handler_error',
  event: {},
  errorContext: {},
});

describe('listRecordKeys', () => {
  it('sorts by LastModified desc and limits', async () => {
    const s3 = fakeS3([
      {
        Contents: [
          obj('zankyo/fn/2026/09/20/a.json', '2026-09-20T00:00:00Z'),
          obj('zankyo/fn/2026/09/22/c.json', '2026-09-22T00:00:00Z'),
          obj('zankyo/fn/2026/09/21/b.json', '2026-09-21T00:00:00Z'),
        ],
      },
    ]);
    const refs = await listRecordKeys(s3, 'bkt', { limit: 2 });
    assert.equal(refs.length, 2);
    assert.equal(refs[0]?.key, 'zankyo/fn/2026/09/22/c.json');
    assert.equal(refs[1]?.key, 'zankyo/fn/2026/09/21/b.json');
  });

  it('paginates until no token', async () => {
    const s3 = fakeS3([
      {
        Contents: [obj('zankyo/fn/2026/09/22/a.json')],
        IsTruncated: true,
        NextContinuationToken: 't2',
      },
      { Contents: [obj('zankyo/fn/2026/09/22/b.json')] },
    ]);
    const refs = await listRecordKeys(s3, 'bkt', {});
    assert.equal(refs.length, 2);
  });

  it('filters by since', async () => {
    const s3 = fakeS3([
      {
        Contents: [
          obj('zankyo/fn/2026/09/22/new.json', '2099-01-01T00:00:00Z'),
          obj('zankyo/fn/2026/09/22/old.json', '2000-01-01T00:00:00Z'),
        ],
      },
    ]);
    const refs = await listRecordKeys(s3, 'bkt', { since: new Date('2020-01-01') });
    assert.equal(refs.length, 1);
    assert.ok(refs[0]?.key.includes('new'));
  });
});

describe('resolveRecordKey', () => {
  it('resolves --last to the newest key', async () => {
    const s3 = fakeS3([
      { Contents: [obj('zankyo/fn/2026/09/22/latest.json', '2026-09-22T01:00:00Z')] },
    ]);
    const key = await resolveRecordKey(s3, 'bkt', { last: true });
    assert.equal(key, 'zankyo/fn/2026/09/22/latest.json');
  });

  it('finds a requestId by suffix match across pages', async () => {
    const s3 = fakeS3([
      {
        Contents: [obj('zankyo/fn/2026/09/22/other.json')],
        IsTruncated: true,
        NextContinuationToken: 't2',
      },
      { Contents: [obj('zankyo/fn/2026/09/22/target-id.json')] },
    ]);
    const key = await resolveRecordKey(s3, 'bkt', { requestId: 'target-id' });
    assert.equal(key, 'zankyo/fn/2026/09/22/target-id.json');
  });

  it('throws exitCode 4 when not found', async () => {
    const s3 = fakeS3([{ Contents: [] }]);
    await assert.rejects(
      () => resolveRecordKey(s3, 'bkt', { requestId: 'missing' }),
      (e: unknown) => e instanceof CliError && e.exitCode === 4,
    );
  });
});

describe('fetchRecord', () => {
  it('parses the stored record', async () => {
    const s3 = fakeS3([], VALID_RECORD);
    const rec = await fetchRecord(s3, 'bkt', 'zankyo/fn/2026/09/22/r1.json');
    assert.equal(rec.requestId, 'r1');
  });

  it('throws on schema mismatch', async () => {
    const s3 = fakeS3([], '{"hello":1}');
    await assert.rejects(() => fetchRecord(s3, 'bkt', 'k'), CliError);
  });
});
