import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { LambdaClient } from '@aws-sdk/client-lambda';
import { S3Client } from '@aws-sdk/client-s3';
import { envTimeout, makeClients } from '../src/aws.ts';

describe('envTimeout', () => {
  it('returns fallback when unset or empty', () => {
    delete process.env.ZANKYO_TEST_TMO;
    assert.equal(envTimeout('ZANKYO_TEST_TMO', 100), 100);
    process.env.ZANKYO_TEST_TMO = '';
    assert.equal(envTimeout('ZANKYO_TEST_TMO', 100), 100);
  });

  it('parses positive integers and rejects garbage', () => {
    process.env.ZANKYO_TEST_TMO = '2500';
    assert.equal(envTimeout('ZANKYO_TEST_TMO', 100), 2500);
    process.env.ZANKYO_TEST_TMO = 'abc';
    assert.equal(envTimeout('ZANKYO_TEST_TMO', 100), 100);
    process.env.ZANKYO_TEST_TMO = '-5';
    assert.equal(envTimeout('ZANKYO_TEST_TMO', 100), 100);
    delete process.env.ZANKYO_TEST_TMO;
  });
});

describe('makeClients', () => {
  it('propagates --profile to the AWS provider chain', () => {
    const saved = process.env.AWS_PROFILE;
    try {
      delete process.env.AWS_PROFILE;
      makeClients({ profile: 'sandbox' });
      assert.equal(process.env.AWS_PROFILE, 'sandbox');
    } finally {
      if (saved === undefined) {
        delete process.env.AWS_PROFILE;
      } else {
        process.env.AWS_PROFILE = saved;
      }
    }
  });

  it('passes --region to the S3 and Lambda clients', async (t) => {
    const regions: string[] = [];
    async function captureRegion(this: { config: { region: () => Promise<string> } }) {
      regions.push(await this.config.region());
      return {};
    }
    t.mock.method(S3Client.prototype, 'send', captureRegion);
    t.mock.method(LambdaClient.prototype, 'send', captureRegion);
    const clients = makeClients({ region: 'sa-east-1' });
    await clients.s3.listObjectsV2({ Bucket: 'b', Prefix: 'zankyo/' });
    await clients.lambda.invoke({ FunctionName: 'fn', Payload: new Uint8Array() });
    assert.deepEqual(regions, ['sa-east-1', 'sa-east-1']);
  });
});
