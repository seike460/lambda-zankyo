/**
 * AWS クライアントの生成。--profile/--region の解釈はここだけ。
 * IO 境界を一箇所にして、コマンド本体から SDK 初期化を追い出す。
 */
import { LambdaClient } from '@aws-sdk/client-lambda';
import { S3Client } from '@aws-sdk/client-s3';
import { strVal } from './args.ts';

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
  const cfg = region ? { region } : {};
  return {
    s3: new S3Client(cfg),
    lambda: new LambdaClient(cfg),
    region,
  };
}
