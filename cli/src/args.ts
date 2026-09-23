/**
 * コマンド引数の解釈。node:util の parseArgs を薄く包み、
 * 全コマンド共通フラグ（--profile/--region/--bucket/--json）と
 * 値の型付き取り出しをここに集約する。
 */
import { type ParseArgsConfig, parseArgs } from 'node:util';
import { CliError } from './errors.ts';

export type OptionDefs = ParseArgsConfig['options'];

/** 全コマンドで共有するフラグ定義。各コマンドの options にスプレッドして使う。 */
export const SHARED_OPTIONS: OptionDefs = {
  region: { type: 'string' },
  profile: { type: 'string' },
  bucket: { type: 'string' },
  json: { type: 'boolean', default: false },
  help: { type: 'boolean', short: 'h', default: false },
};

export interface Parsed {
  values: Record<string, unknown>;
  positionals: string[];
}

export function parseCliArgs(argv: string[], options: OptionDefs): Parsed {
  try {
    const { values, positionals } = parseArgs({
      args: argv,
      options,
      allowPositionals: true,
      strict: true,
    });
    return { values, positionals };
  } catch (e) {
    throw new CliError(e instanceof Error ? e.message : String(e), 2, 'run with --help');
  }
}

export function strVal(v: unknown): string | undefined {
  return typeof v === 'string' ? v : undefined;
}

export function strList(v: unknown): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
}

/** --bucket フラグ > ZANKYO_BUCKET 環境変数。どちらも無ければエラー。 */
export function resolveBucket(values: Record<string, unknown>): string {
  const fromFlag = strVal(values.bucket);
  const fromEnv = process.env.ZANKYO_BUCKET;
  const bucket = fromFlag ?? fromEnv;
  if (!bucket) {
    throw new CliError(
      'bucket is not specified',
      2,
      'pass --bucket or set ZANKYO_BUCKET (the value used by the layer)',
    );
  }
  return bucket;
}

const SINCE_UNITS: Record<string, number> = {
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 604_800_000,
};

/** "24h"・"7d" のような相対時刻をミリ秒に変換する。 */
export function parseSince(text: string): number {
  const m = /^(\d+)\s*([smhdw])$/.exec(text.trim());
  const unit = m?.[2] ? SINCE_UNITS[m[2]] : undefined;
  if (!m?.[1] || unit === undefined) {
    throw new CliError(`invalid --since value: ${text}`, 2, 'use e.g. 30m, 24h, 7d');
  }
  return Number(m[1]) * unit;
}

export function requirePositional(positionals: string[], index: number, name: string): string {
  const v = positionals[index];
  if (v === undefined) {
    throw new CliError(`missing required argument: ${name}`, 2, 'see command usage');
  }
  return v;
}
