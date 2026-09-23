import type { LambdaClient } from '@aws-sdk/client-lambda';
import type { S3Client } from '@aws-sdk/client-s3';
import type { AwsClients } from '../src/aws.ts';

/** コマンド名ごとに応答を返す最小フェイク。
 *  send() の引数は Command インスタンス（input は .input）。 */
export function fakeS3(listPages: Record<string, unknown>[], getBody?: string): S3Client {
  const fake = {
    calls: 0,
    async send(command: unknown): Promise<unknown> {
      const name = (command as { constructor: { name: string } }).constructor.name;
      if (name === 'ListObjectsV2Command') {
        const page = listPages[Math.min(fake.calls, listPages.length - 1)];
        fake.calls += 1;
        return page;
      }
      if (name === 'GetObjectCommand') {
        return { Body: { transformToString: async () => getBody ?? '{}' } };
      }
      throw new Error(`unexpected command ${name}`);
    },
  };
  return fake as unknown as S3Client;
}

export function fakeLambda(out: {
  StatusCode?: number;
  FunctionError?: string;
  Payload?: Uint8Array;
}): LambdaClient {
  return { send: async () => out } as unknown as LambdaClient;
}

/** 逐次応答を変えたい場合用（diff の 2 alias 等）。 */
export function fakeLambdaSeq(outs: unknown[]): LambdaClient {
  const fake = {
    calls: 0,
    async send(): Promise<unknown> {
      return outs[Math.min(fake.calls++, outs.length - 1)];
    },
  };
  return fake as unknown as LambdaClient;
}

export function deps(s3: S3Client, lambda?: LambdaClient): AwsClients {
  return {
    s3,
    lambda: lambda ?? ({} as LambdaClient),
    region: undefined,
  };
}

/** console.log を捕捉してコマンドの出力を検証する。 */
export async function captureStdout(
  fn: () => Promise<number>,
): Promise<{ code: number; out: string }> {
  const orig = console.log;
  let out = '';
  console.log = (...args: unknown[]) => {
    out += `${args.map(String).join(' ')}\n`;
  };
  try {
    const code = await fn();
    return { code, out };
  } finally {
    console.log = orig;
  }
}

export const VALID_RECORD = JSON.stringify({
  version: '1',
  functionName: 'fn',
  functionVersion: '1',
  requestId: 'r1',
  invokedAt: '2026-09-22T00:00:00Z',
  failureType: 'handler_error',
  event: { user: 'alice', password: 'x***' },
  errorContext: { errorType: 'Error', errorMessage: 'boom' },
});
