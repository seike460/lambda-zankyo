/**
 * 2 つの応答ペイロードの構造差分。純粋関数。
 * `zankyo diff` が exit code で一致/差異を返せるよう、
 * 差分は { path, kind } の列として決定的な順序で返す。
 */
export interface JsonDiff {
  path: string;
  kind: 'added' | 'removed' | 'changed';
  a: unknown;
  b: unknown;
}

export function diffJson(a: unknown, b: unknown, path = '$'): JsonDiff[] {
  if (Object.is(a, b)) return [];
  if (isPlainObject(a) && isPlainObject(b)) {
    return diffObjects(a as Record<string, unknown>, b as Record<string, unknown>, path);
  }
  if (Array.isArray(a) && Array.isArray(b)) {
    return diffArrays(a, b, path);
  }
  return [{ path, kind: 'changed', a, b }];
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function diffObjects(
  a: Record<string, unknown>,
  b: Record<string, unknown>,
  path: string,
): JsonDiff[] {
  const diffs: JsonDiff[] = [];
  const keys = [...new Set([...Object.keys(a), ...Object.keys(b)])].sort();
  for (const k of keys) {
    const p = `${path}.${k}`;
    const inA = Object.hasOwn(a, k);
    const inB = Object.hasOwn(b, k);
    if (inA && !inB) {
      diffs.push({ path: p, kind: 'removed', a: a[k], b: undefined });
    } else if (!inA && inB) {
      diffs.push({ path: p, kind: 'added', a: undefined, b: b[k] });
    } else {
      diffs.push(...diffJson(a[k], b[k], p));
    }
  }
  return diffs;
}

function diffArrays(a: unknown[], b: unknown[], path: string): JsonDiff[] {
  const diffs: JsonDiff[] = [];
  const n = Math.max(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const p = `${path}[${i}]`;
    if (i >= a.length) {
      diffs.push({ path: p, kind: 'added', a: undefined, b: b[i] });
    } else if (i >= b.length) {
      diffs.push({ path: p, kind: 'removed', a: a[i], b: undefined });
    } else {
      diffs.push(...diffJson(a[i], b[i], p));
    }
  }
  return diffs;
}

/** キー順を安定化した JSON 文字列。比較・表示用。 */
export function canonicalize(v: unknown): string {
  return JSON.stringify(sortKeys(v), null, 2);
}

function sortKeys(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(sortKeys);
  if (isPlainObject(v)) {
    const out: Record<string, unknown> = {};
    for (const k of Object.keys(v).sort()) out[k] = sortKeys(v[k]);
    return out;
  }
  return v;
}

const KIND_MARK: Record<JsonDiff['kind'], string> = {
  added: '+',
  removed: '-',
  changed: '~',
};

export function formatDiffs(diffs: JsonDiff[]): string {
  if (diffs.length === 0) return '(no differences)';
  return diffs
    .map((d) => {
      const av = d.kind === 'added' ? '' : ` ${JSON.stringify(d.a)}`;
      const bv = d.kind === 'removed' ? '' : ` ${JSON.stringify(d.b)}`;
      return `${KIND_MARK[d.kind]} ${d.path}:${av} ->${bv}`;
    })
    .join('\n');
}
