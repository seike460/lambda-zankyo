import {
  parseCliArgs,
  requirePositional,
  resolveBucket,
  SHARED_OPTIONS,
  strVal,
} from '../args-bundle.ts';
import { makeClients } from '../aws.ts';
import { invokeFunction, qualifiedName } from '../invoke.ts';
import { jsonOut } from '../output.ts';
import { fetchRecord, resolveRecordKey } from '../store.ts';

const USAGE = `zankyo replay — re-invoke the recorded event against a function version

usage: zankyo replay <requestId> [--alias NAME] [--function NAME] [--bucket B] [--json]
`;

export async function run(argv: string[]): Promise<number> {
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
  const { s3, lambda } = makeClients(values);
  const key = await resolveRecordKey(s3, bucket, {
    requestId,
    functionName: strVal(values.function),
  });
  const rec = await fetchRecord(s3, bucket, key);
  const target = qualifiedName(rec.functionName, strVal(values.alias));
  const out = await invokeFunction(lambda, target, rec.event);
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
    return 0;
  }
  console.log(`target: ${target}`);
  console.log(`status: ${out.statusCode}${out.functionError ? ` (${out.functionError})` : ''}`);
  console.log(out.payloadText);
  return 0;
}
