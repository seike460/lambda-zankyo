/**
 * CLI の失敗を exit code つきで表すエラー型。
 * 利用者の操作ミス（引数不正・設定不足）は 2、AWS 側の失敗は 3、
 * レコードの不在・形式不正・再現できないレコードは 4 とし、
 * シェルから原因を分岐できるようにする。
 */
export class CliError extends Error {
  readonly exitCode: number;
  readonly hint: string | undefined;

  constructor(message: string, exitCode = 2, hint?: string) {
    super(message);
    this.name = 'CliError';
    this.exitCode = exitCode;
    this.hint = hint;
  }
}

/** unknown の例外からメッセージ文字列を取り出す。 */
export function errMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
