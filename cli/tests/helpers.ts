import type { AwsClients } from '../src/aws.ts';
import type {
  FunctionInvoker,
  GetInput,
  InvokeInput,
  InvokeResult,
  ListInput,
  RecordListPage,
  RecordReader,
} from '../src/ports.ts';

/**
 * コマンドテスト用フェイク。ports.ts の構造的ポートをそのまま満たす
 * ので SDK 型への cast は一切要らない。受け取った入力は配列に残し、
 * テストから ContinuationToken・Prefix・Key・FunctionName を検証できるようにする。
 */
const EMPTY_PAGE: RecordListPage = {};
const EMPTY_RESULT: InvokeResult = {};

export interface FakeS3 extends RecordReader {
  readonly listInputs: ListInput[];
  readonly getInputs: GetInput[];
}

export interface FakeLambda extends FunctionInvoker {
  readonly inputs: InvokeInput[];
}

export function fakeS3(listPages: RecordListPage[], getBody?: string): FakeS3 {
  const listInputs: ListInput[] = [];
  const getInputs: GetInput[] = [];
  return {
    listInputs,
    getInputs,
    async listObjectsV2(input) {
      const page = listPages[Math.min(listInputs.length, listPages.length - 1)] ?? EMPTY_PAGE;
      listInputs.push(input);
      return page;
    },
    async getObject(input) {
      getInputs.push(input);
      return { Body: { transformToString: async () => getBody ?? '{}' } };
    },
  };
}

export function fakeLambda(out: InvokeResult): FakeLambda {
  return fakeLambdaSeq([out]);
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
export function fakeLambdaSeq(outs: InvokeResult[]): FakeLambda {
  const inputs: InvokeInput[] = [];
  return {
    inputs,
    async invoke(input) {
      const out = outs[Math.min(inputs.length, outs.length - 1)] ?? EMPTY_RESULT;
      inputs.push(input);
      return out;
    },
  };
}

/** AWS API の失敗（exit 3 の経路）を再現する。 */
export function rejecting(message: string): () => Promise<never> {
  return async () => {
    throw new Error(message);
  };
}

/** 送られたペイロードを JSON として読み戻す。 */
export function sentPayload(input: InvokeInput | undefined): unknown {
  return input ? JSON.parse(new TextDecoder().decode(input.Payload)) : undefined;
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
