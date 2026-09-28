import type { AwsClients } from '../src/aws.ts';
import type {
  FunctionInvoker,
  InvokeInput,
  InvokeResult,
  RecordListPage,
  RecordReader,
} from '../src/ports.ts';

/**
 * コマンドテスト用フェイク。ports.ts の構造的ポートをそのまま満たす
 * ので SDK 型への cast は一切要らない。
 */
const EMPTY_PAGE: RecordListPage = {};
const EMPTY_RESULT: InvokeResult = {};

export function fakeS3(listPages: RecordListPage[], getBody?: string): RecordReader {
  let calls = 0;
  return {
    async listObjectsV2() {
      const page = listPages[Math.min(calls, listPages.length - 1)] ?? EMPTY_PAGE;
      calls += 1;
      return page;
    },
    async getObject() {
      return { Body: { transformToString: async () => getBody ?? '{}' } };
    },
  };
}

export function fakeLambda(out: InvokeResult): FunctionInvoker {
  return {
    async invoke() {
      return out;
    },
  };
}

/** 渡された InvokeInput を検査したい場合用。 */
export function fakeLambdaHandler(handler: (input: InvokeInput) => InvokeResult): FunctionInvoker {
  return {
    async invoke(input) {
      return handler(input);
    },
  };
}

/** 逐次応答を変えたい場合用（diff の 2 alias 等）。 */
export function fakeLambdaSeq(outs: InvokeResult[]): FunctionInvoker {
  let calls = 0;
  return {
    async invoke() {
      return outs[Math.min(calls++, outs.length - 1)] ?? EMPTY_RESULT;
    },
  };
}

export function deps(s3: RecordReader, lambda?: FunctionInvoker): AwsClients {
  return {
    s3,
    lambda: lambda ?? fakeLambda({}),
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
