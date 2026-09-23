import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { CliError } from '../src/errors.ts';

describe('CliError', () => {
  it('carries exitCode and optional hint', () => {
    const e = new CliError('boom', 4, 'try list');
    assert.equal(e.exitCode, 4);
    assert.equal(e.hint, 'try list');
    assert.equal(e.name, 'CliError');
    assert.ok(e instanceof Error);
  });

  it('defaults exitCode to 2 without hint', () => {
    const e = new CliError('bad args');
    assert.equal(e.exitCode, 2);
    assert.equal(e.hint, undefined);
  });
});
