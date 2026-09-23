import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import type { InvokeCommand, LambdaClient } from '@aws-sdk/client-lambda';
import { invokeFunction, qualifiedName } from '../src/invoke.ts';
import { fakeLambda } from './helpers.ts';

describe('qualifiedName', () => {
  it('appends alias with colon', () => {
    assert.equal(qualifiedName('fn'), 'fn');
    assert.equal(qualifiedName('fn', 'v2'), 'fn:v2');
  });
});

describe('invokeFunction', () => {
  it('decodes JSON payloads', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      Payload: new TextEncoder().encode('{"ok":true}'),
    });
    const out = await invokeFunction(lambda, 'fn', { a: 1 });
    assert.equal(out.statusCode, 200);
    assert.deepEqual(out.payload, { ok: true });
    assert.equal(out.functionError, undefined);
  });

  it('keeps non-JSON payloads as text', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      Payload: new TextEncoder().encode('plain text'),
    });
    const out = await invokeFunction(lambda, 'fn', {});
    assert.equal(out.payload, 'plain text');
    assert.equal(out.payloadText, 'plain text');
  });

  it('surfaces FunctionError', async () => {
    const lambda = fakeLambda({
      StatusCode: 200,
      FunctionError: 'Handled',
      Payload: new TextEncoder().encode('{"errorMessage":"x"}'),
    });
    const out = await invokeFunction(lambda, 'fn', {});
    assert.equal(out.functionError, 'Handled');
  });

  it('sends an InvokeCommand with the target and event', async () => {
    let seenName: string | undefined;
    let seenPayload: string | undefined;
    const lambda = {
      send: async (command: InvokeCommand) => {
        seenName = command.input.FunctionName;
        const p = command.input.Payload;
        seenPayload = p instanceof Uint8Array ? new TextDecoder().decode(p) : undefined;
        return { StatusCode: 200, Payload: new TextEncoder().encode('{}') };
      },
    } as unknown as LambdaClient;
    await invokeFunction(lambda, 'fn:prod', { ping: 1 });
    assert.equal(seenName, 'fn:prod');
    assert.equal(seenPayload, '{"ping":1}');
  });
});
