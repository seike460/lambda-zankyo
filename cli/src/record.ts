/**
 * S3 に保存される失敗レコードの型とパース。
 * スキーマは proxy/src/record.rs の FailureRecord と一致させる。
 */
import { CliError } from './errors.ts';

const KNOWN_FAILURE_TYPES = ['handler_error', 'init_error', 'timeout'] as const;

/**
 * proxy が現在書き出す failureType 値。wire 上は opaque string として
 * 扱い、新バージョンが追加した型も旧 CLI で読める（前方互換）。
 * 既知集合に型を追加する場合は KNOWN_FAILURE_TYPES だけを更新する。
 */
export type FailureType = (typeof KNOWN_FAILURE_TYPES)[number];

/** S3 キーの先頭セグメント。proxy/src/record.rs の KEY_PREFIX と揃える。 */
export const RECORD_PREFIX = 'zankyo';

export interface ErrorContext {
  errorType?: string;
  errorMessage?: string;
  stackTrace?: string;
}

export interface ScrubReport {
  fieldsRedacted: number;
  patternsApplied: string[];
}

export interface ZankyoRecord {
  version: string;
  functionName: string;
  functionVersion: string;
  requestId: string;
  invokedAt: string;
  failureType: string;
  event: unknown;
  response?: unknown;
  errorContext: ErrorContext;
  /** event が非 JSON ボディの生テキストのとき true（proxy が記録）。 */
  eventIsRawText?: boolean;
  /** event が UTF-8 でないバイナリのとき true。event は base64 文字列。 */
  eventIsBase64?: boolean;
  scrubReport?: ScrubReport;
  truncated?: boolean;
}

const KNOWN_FAILURE_TYPE_SET: ReadonlySet<string> = new Set(KNOWN_FAILURE_TYPES);

/** 既知でない failureType は警告対象（読み込み自体は前方互換で通す）。 */
export function isKnownFailureType(v: string): v is FailureType {
  return KNOWN_FAILURE_TYPE_SET.has(v);
}

export function isRecord(v: unknown): v is ZankyoRecord {
  if (typeof v !== 'object' || v === null) return false;
  if (
    !(
      'version' in v &&
      'functionName' in v &&
      'requestId' in v &&
      'invokedAt' in v &&
      'failureType' in v &&
      'event' in v
    )
  ) {
    return false;
  }
  return (
    typeof v.version === 'string' &&
    typeof v.functionName === 'string' &&
    typeof v.requestId === 'string' &&
    typeof v.invokedAt === 'string' &&
    typeof v.failureType === 'string'
  );
}

export function parseRecord(text: string, source: string): ZankyoRecord {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new CliError(`record at ${source} is not valid JSON`, 4);
  }
  if (!isRecord(v)) {
    throw new CliError(`record at ${source} does not match the zankyo schema`, 4);
  }
  return v;
}

export interface ParsedKey {
  functionName: string;
  requestId: string;
}

/**
 * `zankyo/{fn}/{yyyy}/{mm}/{dd}/{requestId}.json` を分解する。
 * キー規約は proxy/src/record.rs の s3_key と一致させる。
 */
export function parseRecordKey(key: string): ParsedKey | null {
  const parts = key.split('/');
  if (parts.length !== 6 || parts[0] !== RECORD_PREFIX || !parts[5]?.endsWith('.json')) {
    return null;
  }
  const functionName = parts[1];
  const requestId = parts[5].slice(0, -'.json'.length);
  if (!functionName || !requestId) return null;
  return { functionName, requestId };
}

export function keyMatchesRequestId(key: string, requestId: string): boolean {
  return key.endsWith(`/${requestId}.json`);
}
