import { parseCliArgs, requirePositional, resolveBucket, SHARED_OPTIONS, strVal } from '../args.ts';
import { type AwsClients, makeClients } from '../aws.ts';
import { buildFixtureEvent } from '../fixture.ts';
import { invokeFunction, qualifiedName } from '../invoke.ts';
import { jsonOut } from '../output.ts';
import { loadRecord } from '../store.ts';

const USAGE = `zankyo replay — re-invoke the recorded event against a function version

usage: zankyo replay <requestId> [--alias NAME] [--function NAME] [--bucket B] [--json]
`;

export async function run(argv: string[], deps?: AwsClients): Promise<number> {
  const { values, positionals } = parseCliArgs(argv, {
    ...SHARED_OPTIONS,
    function: { type: 'string' },
    alias: { type: 'string' },
  });
  if (values.help) {
    console.log(USAGE);
    return 0;
  }
  const requestId = requirePositional(positionals, 0, 'requestId');
  const bucket = resolveBucket(values);
  const { s3, lambda } = deps ?? makeClients(values);
  const rec = await loadRecord(s3, bucket, {
    requestId,
    functionName: strVal(values.function),
  });
  const target = qualifiedName(rec.functionName, strVal(values.alias));
  // 元イベントが残っていないレコード（truncated / event 欠落）を
  // 投げても結果は意味を持たないので、fixture 経路と同じ検証で止める
  const out = await invokeFunction(lambda, target, buildFixtureEvent(rec));
  if (values.json) {
    console.log(
      jsonOut({
        requestId: rec.requestId,
        target,
        statusCode: out.statusCode,
        functionError: out.functionError,
        payload: out.payload,
      }),
    );
  } else {
    console.log(`target: ${target}`);
    console.log(`status: ${out.statusCode}${out.functionError ? ` (${out.functionError})` : ''}`);
    console.log(out.payloadText);
  }
  // redrive と同じく、関数が今回もエラーを返したら非ゼロ —
  // 「修正が効いたか」を CI の exit code で見られるようにする
  return out.functionError ? 1 : 0;
}
