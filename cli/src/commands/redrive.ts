import { parseCliArgs, requirePositional, resolveBucket, SHARED_OPTIONS, strVal } from '../args.ts';
import { type AwsClients, makeClients } from '../aws.ts';
import { buildFixtureEvent } from '../fixture.ts';
import { invokeFunction, qualifiedName } from '../invoke.ts';
import { jsonOut } from '../output.ts';
import { loadRecord } from '../store.ts';

const USAGE = `zankyo redrive — re-invoke the production function with the recorded event

usage: zankyo redrive <requestId> [--function NAME] [--alias NAME] [--confirm] [--bucket B] [--json]
default is dry-run; pass --confirm to actually invoke
`;

export async function run(argv: string[], deps?: AwsClients): Promise<number> {
  const { values, positionals } = parseCliArgs(argv, {
    ...SHARED_OPTIONS,
    function: { type: 'string' },
    alias: { type: 'string' },
    confirm: { type: 'boolean', default: false },
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
  if (values.confirm !== true) {
    const summary = {
      dryRun: true,
      requestId: rec.requestId,
      target,
      failureType: rec.failureType,
      note: 'pass --confirm to invoke',
    };
    console.log(values.json ? jsonOut(summary) : formatSummary(summary));
    return 0;
  }
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
    console.log(`redrove ${rec.requestId} -> ${target}`);
    console.log(`status: ${out.statusCode}${out.functionError ? ` (${out.functionError})` : ''}`);
    console.log(out.payloadText);
  }
  // replay と同じく、関数が今回もエラーを返したら非ゼロ
  return out.functionError ? 1 : 0;
}

function formatSummary(s: Record<string, unknown>): string {
  return [
    'dry-run (no invocation performed)',
    `  requestId:   ${s.requestId}`,
    `  target:      ${s.target}`,
    `  failureType: ${s.failureType}`,
    `  ${s.note}`,
  ].join('\n');
}
