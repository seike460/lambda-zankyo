/**
 * Lambda invoke の薄いラッパー。応答ペイロードを JSON として解釈しつつ、
 * JSON でない応答も失わないように生テキストを併せて返す。
 */
import { CliError, errMessage } from './errors.ts';
import type { FunctionInvoker } from './ports.ts';

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
  lambda: FunctionInvoker,
  functionTarget: string,
  event: unknown,
): Promise<InvokeOutcome> {
  const out = await lambda
    .invoke({
      FunctionName: functionTarget,
      Payload: new TextEncoder().encode(JSON.stringify(event ?? {})),
    })
    .catch((e) => {
      // S3 側と同じく AWS 失敗は exit 3 で統一する（README の exit code 表）
      throw new CliError(`failed to invoke ${functionTarget}: ${errMessage(e)}`, 3);
    });
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
