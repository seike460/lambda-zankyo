import { writeFile } from 'node:fs/promises';
import { parseCliArgs, requirePositional, resolveBucket, SHARED_OPTIONS, strVal } from '../args.ts';
import { type AwsClients, makeClients } from '../aws.ts';
import { fixtureJson } from '../fixture.ts';
import { loadRecord } from '../store.ts';

const USAGE = `zankyo fixture — write a sam local invoke -e event file from a failure record

usage: zankyo fixture <requestId|--last> [--function NAME] [--out FILE] [--bucket B]
`;

export async function run(argv: string[], deps?: AwsClients): Promise<number> {
  const { values, positionals } = parseCliArgs(argv, {
    ...SHARED_OPTIONS,
    function: { type: 'string' },
    out: { type: 'string' },
    last: { type: 'boolean', default: false },
  });
  if (values.help) {
    console.log(USAGE);
    return 0;
  }
  const last = values.last === true;
  const requestId = last ? undefined : requirePositional(positionals, 0, 'requestId|--last');
  const bucket = resolveBucket(values);
  const { s3 } = deps ?? makeClients(values);
  const rec = await loadRecord(s3, bucket, {
    requestId,
    last,
    functionName: strVal(values.function),
  });
  const body = fixtureJson(rec);
  // 非 JSON イベントは sam local invoke -e も Lambda の Invoke API も（JSON 前提）
  // 受け付けない。そのまま書き出すが、用途を誤解させないよう stderr で断っておく。
  if (rec.eventIsRawText || rec.eventIsBase64) {
    console.error(
      'note: this record holds a non-JSON event, written as-is; `sam local invoke -e` and `zankyo replay`/`diff`/`redrive` accept only JSON events',
    );
  }
  const out = strVal(values.out);
  if (out) {
    // scrub 済みでもイベントの断片を含むため、新規作成時は所有者だけが読めるようにする
    await writeFile(out, body, { mode: 0o600 });
    console.log(`wrote ${out} — run: sam local invoke -e ${out}`);
  } else {
    process.stdout.write(body);
  }
  return 0;
}
