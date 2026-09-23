/**
 * 出力整形。テキスト表と JSON の両対応（--json）。
 * 純粋関数なのでテストは入出力だけ見る。
 */
export function table(headers: string[], rows: string[][]): string {
  const widths = headers.map((h, i) => Math.max(h.length, ...rows.map((r) => (r[i] ?? '').length)));
  const render = (cells: string[]) =>
    cells
      .map((c, i) => (c ?? '').padEnd(widths[i] ?? 0))
      .join('  ')
      .trimEnd();
  const sep = widths.map((w) => '-'.repeat(w));
  return [render(headers), render(sep), ...rows.map(render)].join('\n');
}

export function jsonOut(v: unknown): string {
  return JSON.stringify(v, null, 2);
}
