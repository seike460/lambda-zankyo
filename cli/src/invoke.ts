/**
 * Lambda invoke の薄いラッパー。応答ペイロードを JSON として解釈しつつ、
 * JSON でない応答も失わないように生テキストを併せて返す。
 */
import { InvokeCommand, type LambdaClient } from '@aws-sdk/client-lambda';

export interface InvokeOutcome {
  statusCode: number | undefined;
  functionError: string | undefined;
  /** JSON として解釈できた場合の値。差分比較はこちらを使う。 */
  payload: unknown;
  /** 生の応答テキスト。表示用。 */
  payloadText: string;
}

export function qualifiedName(functionName: string, alias?: string): string {
  return alias ? `${functionName}:${alias}` : functionName;
}

export async function invokeFunction(
  lambda: LambdaClient,
  functionTarget: string,
  event: unknown,
): Promise<InvokeOutcome> {
  const out = await lambda.send(
    new InvokeCommand({
      FunctionName: functionTarget,
      Payload: new TextEncoder().encode(JSON.stringify(event ?? {})),
      LogType: 'Tail',
    }),
  );
  const payloadText = out.Payload ? new TextDecoder().decode(out.Payload) : '';
  let payload: unknown = payloadText;
  try {
    payload = JSON.parse(payloadText);
  } catch {
    // JSON でない応答はテキストのまま比較・表示する
  }
  return {
    statusCode: out.StatusCode,
    functionError: out.FunctionError,
    payload,
    payloadText,
  };
}
