/**
 * AWS クライアントの生成。--profile/--region の解釈はここだけ。
 * SDK クライアントを ports.ts の構造的ポートへ適合させることで、
 * コマンド本体から SDK 初期化と command 型の両方を追い出す。
 */
import { InvokeCommand, LambdaClient } from '@aws-sdk/client-lambda';
import { GetObjectCommand, ListObjectsV2Command, S3Client } from '@aws-sdk/client-s3';
import { NodeHttpHandler } from '@smithy/node-http-handler';
import { strVal } from './args.ts';
import type {
  FunctionInvoker,
  GetInput,
  InvokeInput,
  InvokeResult,
  ListInput,
  RecordListPage,
  RecordObject,
  RecordReader,
} from './ports.ts';

/**
 * 正の整数の環境変数を読む汎用ヘルパ。未設定・非数値・0 以下・小数・
 * `max` 超えは fallback に倒す（壊れた設定で CLI を止めない）。
 * 小数は切り捨てない。0.5 が 0 になり、件数や上限を 0 にしてしまうため。
 */
export const envNum = (name: string, fallback: number, max = Number.MAX_SAFE_INTEGER): number => {
  const v = process.env[name];
  if (v === undefined || v === '') return fallback;
  const n = Number(v);
  return Number.isSafeInteger(n) && n > 0 && n <= max ? n : fallback;
};

/** Node.js のタイマーが扱える上限（ms）。超えると 1ms で発火する。 */
const TIMER_MAX_MS = 2_147_483_647;

/** タイムアウト系 env の読み取り。タイマーの上限を超える値も fallback に倒す。 */
export const envTimeout = (name: string, fallback: number): number =>
  envNum(name, fallback, TIMER_MAX_MS);

/**
 * 個別の send() 呼び出しへ渡す abort シグナル。
 * NodeHttpHandler の requestTimeout はソケット層の打ち切りで、
 * ボディのストリーミング読み取り完了後まで効かない経路があるため、
 * 呼び出し単位でも同じ値で打ち切る（defense in depth）。
 */
const requestSignal = (): AbortSignal =>
  AbortSignal.timeout(envTimeout('ZANKYO_REQUEST_TIMEOUT_MS', 30_000));

/** S3Client を RecordReader ポートへ適合させる。 */
class S3RecordReader implements RecordReader {
  private readonly client: S3Client;
  constructor(client: S3Client) {
    this.client = client;
  }

  async listObjectsV2(input: ListInput): Promise<RecordListPage> {
    return this.client.send(new ListObjectsV2Command(input), {
      abortSignal: requestSignal(),
    });
  }

  async getObject(input: GetInput): Promise<RecordObject> {
    return this.client.send(new GetObjectCommand(input), { abortSignal: requestSignal() });
  }
}

/** LambdaClient を FunctionInvoker ポートへ適合させる。 */
class LambdaFunctionInvoker implements FunctionInvoker {
  private readonly client: LambdaClient;
  constructor(client: LambdaClient) {
    this.client = client;
  }

  async invoke(input: InvokeInput): Promise<InvokeResult> {
    return this.client.send(new InvokeCommand(input), { abortSignal: requestSignal() });
  }
}

export interface AwsClients {
  s3: RecordReader;
  lambda: FunctionInvoker;
}

export function makeClients(values: Record<string, unknown>): AwsClients {
  const profile = strVal(values.profile);
  if (profile) {
    // SDK v3 の既定プロバイダチェーンが読む環境変数に寄せる。
    // これにより credential-providers への追加依存を避ける。
    process.env.AWS_PROFILE = profile;
  }
  const region = strVal(values.region);
  const requestHandler = new NodeHttpHandler({
    connectionTimeout: envTimeout('ZANKYO_CONNECT_TIMEOUT_MS', 5_000),
    requestTimeout: envTimeout('ZANKYO_REQUEST_TIMEOUT_MS', 30_000),
  });
  const cfg = { requestHandler, ...(region ? { region } : {}) };
  return {
    s3: new S3RecordReader(new S3Client(cfg)),
    lambda: new LambdaFunctionInvoker(new LambdaClient(cfg)),
  };
}
