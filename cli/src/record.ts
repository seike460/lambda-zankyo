/**
 * S3 に保存される失敗レコードの型とパース。
 * スキーマは proxy/src/record.rs の FailureRecord と一致させる。
 */
import { CliError } from './errors.ts';

export type FailureType = 'handler_error' | 'init_error' | 'timeout';

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
  failureType: FailureType;
  event: unknown;
  response?: unknown;
  errorContext: ErrorContext;
  scrubReport?: ScrubReport;
  truncated?: boolean;
}

const FAILURE_TYPES: ReadonlySet<string> = new Set(['handler_error', 'init_error', 'timeout']);

export function isRecord(v: unknown): v is ZankyoRecord {
  if (typeof v !== 'object' || v === null) return false;
  const r = v as Record<string, unknown>;
  return (
    typeof r.version === 'string' &&
    typeof r.functionName === 'string' &&
    typeof r.requestId === 'string' &&
    typeof r.invokedAt === 'string' &&
    typeof r.failureType === 'string' &&
    FAILURE_TYPES.has(r.failureType) &&
    'event' in r
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
  if (parts.length !== 6 || parts[0] !== 'zankyo' || !parts[5]?.endsWith('.json')) {
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
