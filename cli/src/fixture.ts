/**
 * 失敗レコードから `sam local invoke -e` 用のイベント JSON を作る。
 * 純粋関数のみ（ファイル書き出しはコマンド側）。
 */
import { CliError } from './errors.ts';
import type { ZankyoRecord } from './record.ts';

/** sam local invoke -e が期待するのはイベントそのものの JSON。 */
export function buildFixtureEvent(rec: ZankyoRecord): unknown {
  if (rec.truncated) {
    throw new CliError(
      'record is truncated; the stored event is only a prefix and cannot be replayed',
      4,
      'increase ZANKYO_MAX_EVENT_KB on the layer to keep full events',
    );
  }
  if (rec.event === null || rec.event === undefined) {
    throw new CliError('record has no event (init_error); there is nothing to replay', 4);
  }
  return rec.event;
}

export function fixtureJson(rec: ZankyoRecord): string {
  const event = buildFixtureEvent(rec);
  // 非 JSON イベントはレコードに生テキストで入っている。
  // JSON 再エンコードすると原文と異なるペイロードになるため、そのまま書く。
  if (rec.eventIsRawText && typeof event === 'string') {
    return `${event}\n`;
  }
  if (rec.eventIsBase64 && typeof event === 'string') {
    return `${event}\n`;
  }
  return `${JSON.stringify(event, null, 2)}\n`;
}

/**
 * replay/redrive/diff で Lambda へ送るペイロード。
 * JSON イベントはエンコード文字列、eventIsRawText の生テキストは原文のまま、
 * eventIsBase64 は元のバイト列へデコードして返す
 * （文字列化すると元イベントと異なる入力になる）。
 */
export function eventPayload(rec: ZankyoRecord): string | Uint8Array {
  const event = buildFixtureEvent(rec);
  if (rec.eventIsBase64 && typeof event === 'string') {
    return Buffer.from(event, 'base64');
  }
  if (rec.eventIsRawText && typeof event === 'string') {
    return event;
  }
  return JSON.stringify(event ?? {});
}
