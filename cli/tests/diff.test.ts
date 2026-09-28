import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { diffJson } from '../src/diff.ts';

describe('diffJson', () => {
  it('returns empty for identical inputs', () => {
    assert.deepEqual(diffJson({ a: 1, b: [1, 2] }, { a: 1, b: [1, 2] }), []);
    assert.deepEqual(diffJson('x', 'x'), []);
    assert.deepEqual(diffJson(null, null), []);
  });

  it('detects changed leaves', () => {
    const diffs = diffJson({ a: 1 }, { a: 2 });
    assert.deepEqual(diffs, [{ path: '$.a', kind: 'changed', a: 1, b: 2 }]);
  });

  it('detects added and removed keys', () => {
    const diffs = diffJson({ a: 1, gone: true }, { a: 1, added: 2 });
    assert.deepEqual(diffs, [
      { path: '$.added', kind: 'added', a: undefined, b: 2 },
      { path: '$.gone', kind: 'removed', a: true, b: undefined },
    ]);
  });

  it('walks nested objects and arrays', () => {
    const diffs = diffJson({ list: [1, { x: 'a' }] }, { list: [1, { x: 'b' }, 3] });
    assert.deepEqual(diffs, [
      { path: '$.list[1].x', kind: 'changed', a: 'a', b: 'b' },
      { path: '$.list[2]', kind: 'added', a: undefined, b: 3 },
    ]);
  });

  it('reports type changes as changed', () => {
    const diffs = diffJson({ v: [1] }, { v: 'one' });
    assert.deepEqual(diffs, [{ path: '$.v', kind: 'changed', a: [1], b: 'one' }]);
  });
});
