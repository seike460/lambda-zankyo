/**
 * CLI の失敗を exit code つきで表すエラー型。
 * 利用者の操作ミス（引数不正・レコード不在）は 2、AWS 側の失敗は 3、
 * 「見つからない」は 4 とし、シェルから原因を分岐できるようにする。
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

export function requireRecord(text: string): never {
  throw new CliError(`record not found: ${text}`, 4);
}
