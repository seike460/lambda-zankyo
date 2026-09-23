/**
 * S3 上の失敗レコードへのアクセス（薄い IO 層）。
 * キー規約の解釈は record.ts の純粋関数に寄せ、ここは取得だけを担う。
 */
import { GetObjectCommand, ListObjectsV2Command, type S3Client } from '@aws-sdk/client-s3';
import { CliError } from './errors.ts';
import { keyMatchesRequestId, parseRecord, type ZankyoRecord } from './record.ts';

const RECORD_PREFIX = 'zankyo/';
const PAGE_LIMIT = 200;
/** 走査するページの上限。巨大バケットでの無制限スキャンを防ぐ。 */
const MAX_PAGES = 50;
/** レコード1件の読み取り上限（32MiB）。想定外の巨大オブジェクトを
 *  メモリへ読み込まないための防御。 */
const MAX_RECORD_BYTES = 32 * 1024 * 1024;

export interface RecordRef {
  key: string;
  lastModified: Date | undefined;
}

export interface ListOptions {
  functionName?: string | undefined;
  since?: Date | undefined;
  limit?: number | undefined;
}

/** 失敗レコードのキー一覧を新しい順で返す。 */
export async function listRecordKeys(
  s3: S3Client,
  bucket: string,
  opts: ListOptions = {},
): Promise<RecordRef[]> {
  const prefix = opts.functionName ? `${RECORD_PREFIX}${opts.functionName}/` : RECORD_PREFIX;
  const refs: RecordRef[] = [];
  let token: string | undefined;
  do {
    const out = await s3.send(
      new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix, ContinuationToken: token }),
    );
    for (const o of out.Contents ?? []) {
      if (!o.Key) continue;
      if (opts.since && o.LastModified && o.LastModified < opts.since) continue;
      refs.push({ key: o.Key, lastModified: o.LastModified });
    }
    token = out.IsTruncated ? out.NextContinuationToken : undefined;
    // --limit 指定時は一覧用途なので無限ページングを避ける
  } while (token && (!opts.limit || refs.length < opts.limit) && refs.length < PAGE_LIMIT * MAX_PAGES);
  refs.sort((a, b) => (b.lastModified?.getTime() ?? 0) - (a.lastModified?.getTime() ?? 0));
  return opts.limit ? refs.slice(0, opts.limit) : refs;
}

export async function fetchRecord(
  s3: S3Client,
  bucket: string,
  key: string,
): Promise<ZankyoRecord> {
  const out = await s3.send(new GetObjectCommand({ Bucket: bucket, Key: key }));
  if (out.ContentLength !== undefined && out.ContentLength > MAX_RECORD_BYTES) {
    throw new CliError(
      `record too large (${out.ContentLength} bytes): s3://${bucket}/${key}`,
      4,
      'zankyo records are expected to be small; refusing to buffer a huge object',
    );
  }
  const text = await out.Body?.transformToString('utf-8');
  if (text === undefined) {
    throw new CliError(`empty object: s3://${bucket}/${key}`, 4);
  }
  return parseRecord(text, `s3://${bucket}/${key}`);
}

export interface KeyQuery {
  requestId?: string | undefined;
  last?: boolean | undefined;
  functionName?: string | undefined;
}

/**
 * requestId または --last からレコードのキーを解決する。
 * requestId はキーの末尾部分なので prefix リストからの前方一致は使えず、
 * ページングしながら末尾一致で探す。
 */
export async function resolveRecordKey(s3: S3Client, bucket: string, q: KeyQuery): Promise<string> {
  if (q.last) {
    const refs = await listRecordKeys(s3, bucket, {
      functionName: q.functionName,
      limit: 1,
    });
    const first = refs[0];
    if (!first) {
      throw new CliError('no failure records found', 4);
    }
    return first.key;
  }
  const requestId = q.requestId;
  if (!requestId) {
    throw new CliError('either requestId or --last is required', 2);
  }
  const prefix = q.functionName ? `${RECORD_PREFIX}${q.functionName}/` : RECORD_PREFIX;
  let token: string | undefined;
  let pages = 0;
  do {
    const out = await s3.send(
      new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix, ContinuationToken: token }),
    );
    const hit = (out.Contents ?? []).find((o) => o.Key && keyMatchesRequestId(o.Key, requestId));
    if (hit?.Key) return hit.Key;
    token = out.IsTruncated ? out.NextContinuationToken : undefined;
    pages += 1;
  } while (token && pages < MAX_PAGES);
  throw new CliError(
    `record not found for requestId: ${q.requestId}`,
    4,
    'check `zankyo list` for available requestIds',
  );
}

/**
 * キー解決と本体取得をまとめる共通経路。requestId/--last どちらの
 * 指定形でも呼び出し側が分岐を持たなくて済むようにする。
 */
export async function loadRecord(s3: S3Client, bucket: string, q: KeyQuery): Promise<ZankyoRecord> {
  const key = await resolveRecordKey(s3, bucket, q);
  return fetchRecord(s3, bucket, key);
}
