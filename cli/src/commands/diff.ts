import {
  parseCliArgs,
  requirePositional,
  resolveBucket,
  SHARED_OPTIONS,
  strList,
  strVal,
} from '../args-bundle.ts';
import { makeClients } from '../aws.ts';
import { diffJson, formatDiffs } from '../diff.ts';
import { CliError } from '../errors.ts';
import { invokeFunction, qualifiedName } from '../invoke.ts';
import { jsonOut } from '../output.ts';
import { loadRecord } from '../store.ts';

const USAGE = `zankyo diff — invoke the recorded event on two aliases and compare responses

usage: zankyo diff <requestId> --alias A --alias B [--function NAME] [--bucket B] [--json]
exit: 0 = identical, 1 = differences, 2+ = error
`;

export async function run(argv: string[]): Promise<number> {
  const { values, positionals } = parseCliArgs(argv, {
    ...SHARED_OPTIONS,
    function: { type: 'string' },
    alias: { type: 'string', multiple: true },
  });
  if (values.help) {
    console.log(USAGE);
    return 0;
  }
  const requestId = requirePositional(positionals, 0, 'requestId');
  const aliases = strList(values.alias);
  const [a, b] = aliases;
  if (aliases.length !== 2 || !a || !b) {
    throw new CliError('diff requires exactly two --alias flags', 2, USAGE);
  }
  const bucket = resolveBucket(values);
  const { s3, lambda } = makeClients(values);
  const rec = await loadRecord(s3, bucket, {
    requestId,
    functionName: strVal(values.function),
  });
  const [ra, rb] = await Promise.all([
    invokeFunction(lambda, qualifiedName(rec.functionName, a), rec.event),
    invokeFunction(lambda, qualifiedName(rec.functionName, b), rec.event),
  ]);
  const diffs = diffJson(ra.payload, rb.payload);
  if (values.json) {
    console.log(
      jsonOut({
        requestId: rec.requestId,
        targets: [qualifiedName(rec.functionName, a), qualifiedName(rec.functionName, b)],
        identical: diffs.length === 0,
        differences: diffs,
      }),
    );
  } else {
    console.log(`a: ${qualifiedName(rec.functionName, a)}`);
    console.log(`b: ${qualifiedName(rec.functionName, b)}`);
    console.log(formatDiffs(diffs));
  }
  return diffs.length === 0 ? 0 : 1;
}
