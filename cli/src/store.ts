/**
 * S3 上の失敗レコードへのアクセス（薄い IO 層）。
 * キー規約の解釈は record.ts の純粋関数に寄せ、ここは取得だけを担う。
 */
import { envNum } from './aws.ts';
import { CliError, errMessage } from './errors.ts';
import type { RecordListPage, RecordReader } from './ports.ts';
import {
  isKnownFailureType,
  keyMatchesRequestId,
  parseRecord,
  parseRecordKey,
  RECORD_PREFIX,
  type ZankyoRecord,
} from './record.ts';

const RECORD_PREFIX_SLASH = `${RECORD_PREFIX}/`;
/** ListObjectsV2 の 1 ページあたり取得件数。ZANKYO_LIST_PAGE_SIZE で調整可能。 */
const pageSize = () => envNum('ZANKYO_LIST_PAGE_SIZE', 200);
/** 走査するページの上限。巨大バケットでの無制限スキャンを防ぐ。 */
const maxPages = () => envNum('ZANKYO_LIST_MAX_PAGES', 50);
/** レコード1件の読み取り上限（既定 32MiB）。想定外の巨大オブジェクトを
 *  メモリへ読み込まないための防御。ZANKYO_RECORD_MAX_MB で調整可能。 */
const maxRecordBytes = () => envNum('ZANKYO_RECORD_MAX_MB', 32) * 1024 * 1024;

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
  s3: RecordReader,
  bucket: string,
  opts: ListOptions = {},
): Promise<RecordRef[]> {
  const prefix = opts.functionName
    ? `${RECORD_PREFIX_SLASH}${opts.functionName}/`
    : RECORD_PREFIX_SLASH;
  const refs: RecordRef[] = [];
  let token: string | undefined;
  let pages = 0;
  do {
    const out = await listPage(s3, bucket, prefix, token);
    for (const o of out.Contents ?? []) {
      if (!o.Key) continue;
      if (opts.since && o.LastModified && o.LastModified < opts.since) continue;
      refs.push({ key: o.Key, lastModified: o.LastModified });
    }
    token = out.IsTruncated ? out.NextContinuationToken : undefined;
    pages += 1;
    // S3 の list はキー辞書順（= 日付パーティション昇順）なので、
    // 途中で打ち切ると「最新」の判定が最古側のページだけで決まる。
    // --limit 指定時も新しい順に並べてから切るため、
    // 上限ページまでは必ず走査する。上限は --since で除外した分も含めた
    // ページ数で数える（件数で数えると、古いページが続く限り止まらない）。
  } while (token && pages < maxPages());
  if (token) {
    // 打ち切りを黙らせると --last が実際より古いレコードを
    // 「最新」と答える。絞り込み手段を添えて stderr へ警告する。
    console.error(
      `zankyo: listing truncated after ${pages} pages; results may miss newer records — narrow with --function or increase ZANKYO_LIST_MAX_PAGES`,
    );
  }
  refs.sort((a, b) => (b.lastModified?.getTime() ?? 0) - (a.lastModified?.getTime() ?? 0));
  return opts.limit ? refs.slice(0, opts.limit) : refs;
}

/** ListObjectsV2 1 ページ分。SDK 例外は CliError に写して上位で一貫処理する。 */
async function listPage(
  s3: RecordReader,
  bucket: string,
  prefix: string,
  token: string | undefined,
): Promise<RecordListPage> {
  try {
    return await s3.listObjectsV2({
      Bucket: bucket,
      Prefix: prefix,
      MaxKeys: pageSize(),
      ...(token ? { ContinuationToken: token } : {}),
    });
  } catch (e) {
    throw new CliError(`failed to list s3://${bucket}/${prefix}: ${errMessage(e)}`, 3);
  }
}

export async function fetchRecord(
  s3: RecordReader,
  bucket: string,
  key: string,
): Promise<ZankyoRecord> {
  const out = await s3.getObject({ Bucket: bucket, Key: key }).catch((e) => {
    throw new CliError(`failed to read s3://${bucket}/${key}: ${errMessage(e)}`, 3);
  });
  const limit = maxRecordBytes();
  if (out.ContentLength !== undefined && out.ContentLength > limit) {
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
  const rec = parseRecord(text, `s3://${bucket}/${key}`);
  if (!isKnownFailureType(rec.failureType)) {
    // 新しい proxy が追加した failureType も読み進められるが、
    // ユーザーの解釈を助けるため既知でないことは stderr に出す。
    console.error(
      `zankyo: unknown failureType ${JSON.stringify(rec.failureType)} in s3://${bucket}/${key} — newer proxy version?`,
    );
  }
  return rec;
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
export async function resolveRecordKey(
  s3: RecordReader,
  bucket: string,
  q: KeyQuery,
): Promise<string> {
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
  const prefix = q.functionName ? `${RECORD_PREFIX_SLASH}${q.functionName}/` : RECORD_PREFIX_SLASH;
  let token: string | undefined;
  let pages = 0;
  do {
    const out = await listPage(s3, bucket, prefix, token);
    const hit = (out.Contents ?? []).find((o) => o.Key && keyMatchesRequestId(o.Key, requestId));
    if (hit?.Key) return hit.Key;
    token = out.IsTruncated ? out.NextContinuationToken : undefined;
    pages += 1;
  } while (token && pages < maxPages());
  throw new CliError(
    `record not found for requestId: ${q.requestId}`,
    4,
    token
      ? `listing was truncated at ${maxPages()} pages — retry with --function to narrow the scan`
      : 'check `zankyo list` for available requestIds',
  );
}

/**
 * キー解決と本体取得をまとめる共通経路。requestId/--last どちらの
 * 指定形でも呼び出し側が分岐を持たなくて済むようにする。
 */
export async function loadRecord(
  s3: RecordReader,
  bucket: string,
  q: KeyQuery,
): Promise<ZankyoRecord> {
  const key = await resolveRecordKey(s3, bucket, q);
  const rec = await fetchRecord(s3, bucket, key);
  // replay/diff/redrive の invoke 先は本文の functionName で決まる。
  // キーの関数セグメント（--function の絞り込みもここに効く）と食い違う
  // レコードは、別の関数への投入に使わせない。
  if (parseRecordKey(key)?.functionName !== rec.functionName) {
    throw new CliError(
      `record at s3://${bucket}/${key} names function ${JSON.stringify(rec.functionName)}, which does not match its key`,
      4,
      'the record may have been written by another function; inspect it before replaying',
    );
  }
  return rec;
}
