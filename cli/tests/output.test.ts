import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { jsonOut, table } from '../src/output.ts';

describe('table', () => {
  it('aligns columns to the widest cell', () => {
    const out = table(
      ['ID', 'NAME'],
      [
        ['1', 'a'],
        ['long-id', 'bb'],
      ],
    );
    const lines = out.split('\n');
    assert.equal(lines[0], 'ID       NAME');
    assert.equal(lines[2], '1        a');
    assert.equal(lines[3], 'long-id  bb');
  });

  it('handles empty rows', () => {
    const out = table(['A'], []);
    assert.equal(out.split('\n').length, 2);
  });
});

describe('jsonOut', () => {
  it('round-trips JSON', () => {
    assert.deepEqual(JSON.parse(jsonOut({ a: [1, 2] })), { a: [1, 2] });
  });
});
