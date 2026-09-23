/**
 * AWS クライアントの生成。--profile/--region の解釈はここだけ。
 * IO 境界を一箇所にして、コマンド本体から SDK 初期化を追い出す。
 */
import { LambdaClient } from '@aws-sdk/client-lambda';
import { S3Client } from '@aws-sdk/client-s3';
import { NodeHttpHandler } from '@smithy/node-http-handler';
import { strVal } from './args.ts';

/**
 * 外部呼び出しに黙って掛かるタイムアウト。
 * CLI がハングして CI を止めないよう、接続 5s・応答 30s で打ち切る。
 * ZANKYO_CONNECT_TIMEOUT_MS / ZANKYO_REQUEST_TIMEOUT_MS で調整可能。
 */
export const envTimeout = (name: string, fallback: number): number => {
  const v = process.env[name];
  if (v === undefined || v === '') return fallback;
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : fallback;
};

/**
 * 個別の send() 呼び出しへ渡す abort シグナル。
 * NodeHttpHandler の requestTimeout はソケット層の打ち切りで、
 * ボディのストリーミング読み取り完了後まで効かない経路があるため、
 * 呼び出し単位でも同じ値で打ち切る（defense in depth）。
 */
export const requestSignal = (): AbortSignal =>
  AbortSignal.timeout(envTimeout('ZANKYO_REQUEST_TIMEOUT_MS', 30_000));

export interface AwsClients {
  s3: S3Client;
  lambda: LambdaClient;
  region: string | undefined;
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
    s3: new S3Client(cfg),
    lambda: new LambdaClient(cfg),
    region,
  };
}
