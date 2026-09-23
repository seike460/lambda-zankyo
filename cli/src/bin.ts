#!/usr/bin/env node
/**
 * zankyo CLI エントリポイント。
 * コマンドディスパッチとエラー→exit code の変換だけを行い、
 * 実処理は commands/* と src/*.ts のモジュールに委譲する。
 */
import { run as runDiff } from './commands/diff.ts';
import { run as runFixture } from './commands/fixture.ts';
import { run as runList } from './commands/list.ts';
import { run as runRedrive } from './commands/redrive.ts';
import { run as runReplay } from './commands/replay.ts';
import { CliError, errMessage } from './errors.ts';

const VERSION = '0.1.0';

const USAGE = `zankyo ${VERSION} — replay failed synchronous Lambda invocations

usage: zankyo <command> [options]

commands:
  list      list failure records stored in S3
  fixture   export a record as a sam local invoke -e event file
  replay    re-invoke the recorded event on a specific version/alias
  diff      invoke two aliases and compare responses (CI-friendly exit codes)
  redrive   re-invoke the production function (dry-run unless --confirm)

shared flags:
  --bucket B    records bucket (or env ZANKYO_BUCKET)
  --region R    AWS region
  --profile P   AWS profile
  --json        machine-readable output
`;

const COMMANDS: Record<string, (argv: string[]) => Promise<number>> = {
  list: runList,
  fixture: runFixture,
  replay: runReplay,
  diff: runDiff,
  redrive: runRedrive,
};

async function main(): Promise<number> {
  const [cmd, ...rest] = process.argv.slice(2);
  if (!cmd || cmd === '--help' || cmd === '-h') {
    console.log(USAGE);
    return 0;
  }
  if (cmd === '--version' || cmd === '-V') {
    console.log(VERSION);
    return 0;
  }
  const fn = COMMANDS[cmd];
  if (!fn) {
    throw new CliError(`unknown command: ${cmd}`, 2, 'run `zankyo --help`');
  }
  return await fn(rest);
}

main().then(
  (code) => process.exit(code),
  (err: unknown) => {
    if (err instanceof CliError) {
      console.error(`error: ${err.message}`);
      if (err.hint) console.error(`hint: ${err.hint}`);
      process.exit(err.exitCode);
    }
    console.error(`error: ${errMessage(err)}`);
    process.exit(2);
  },
);
