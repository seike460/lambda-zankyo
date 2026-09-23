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
  return `${JSON.stringify(buildFixtureEvent(rec), null, 2)}\n`;
}
