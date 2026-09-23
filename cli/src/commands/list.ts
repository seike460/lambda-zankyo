import {
  parseCliArgs,
  parseRecordKey,
  parseSince,
  resolveBucket,
  SHARED_OPTIONS,
  strVal,
} from '../args-bundle.ts';
import { type AwsClients, makeClients } from '../aws.ts';
import { CliError } from '../errors.ts';
import { jsonOut, table } from '../output.ts';
import { listRecordKeys } from '../store.ts';

const USAGE = `zankyo list — list failure records in S3

usage: zankyo list [--function NAME] [--since 24h] [--limit N] [--bucket B] [--json]
`;

export async function run(argv: string[], deps?: AwsClients): Promise<number> {
  const { values } = parseCliArgs(argv, {
    ...SHARED_OPTIONS,
    function: { type: 'string' },
    since: { type: 'string' },
    limit: { type: 'string', default: '50' },
  });
  if (values.help) {
    console.log(USAGE);
    return 0;
  }
  const limit = Number(strVal(values.limit));
  if (!Number.isInteger(limit) || limit < 1 || limit > 10_000) {
    throw new CliError(`invalid --limit: ${strVal(values.limit)}`);
  }
  const sinceText = strVal(values.since);
  const since = sinceText ? new Date(Date.now() - parseSince(sinceText)) : undefined;
  const bucket = resolveBucket(values);
  const { s3 } = deps ?? makeClients(values);
  const refs = await listRecordKeys(s3, bucket, {
    functionName: strVal(values.function),
    since,
    limit,
  });
  if (values.json) {
    console.log(jsonOut(refs));
    return 0;
  }
  const rows = refs.map((r) => {
    const parsed = parseRecordKey(r.key);
    return [
      parsed?.requestId ?? r.key,
      parsed?.functionName ?? '-',
      r.lastModified?.toISOString() ?? '-',
    ];
  });
  console.log(table(['REQUEST_ID', 'FUNCTION', 'STORED_AT'], rows));
  return 0;
}
