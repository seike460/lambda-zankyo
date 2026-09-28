#!/usr/bin/env node
//! zankyo Layer zip のビルド。x86_64 / aarch64 の musl 静的バイナリを作り、
//! Lambda が /opt に展開するレイアウト（ENTRIES）で zip 化する。
//!
//! 前提: cross (https://github.com/cross-rs/cross) か musl ツールチェーン。
//! `BUILDER="cargo zigbuild"` のように `cargo build` 相当の
//! コマンド行を差し替えられる。
//! `SKIP_BUILD=1` でビルドを省き target/ 済みのバイナリだけ梱包する。
//! 梱包でも THIRD_PARTY_LICENSES の生成に `cargo metadata` を使う。
//! Node 24+ は型注釈を strip してそのまま実行する（ビルド不要）。

import { execFileSync, spawnSync } from 'node:child_process';
import {
  chmodSync,
  cpSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

/// arch 名 → Rust target triple の写像。新しい arch はここに 1 行足す。
const TARGETS: Record<string, string> = {
  x86_64: 'x86_64-unknown-linux-musl',
  aarch64: 'aarch64-unknown-linux-musl',
};

const EXECUTABLES = ['bin/zankyo', 'extensions/zankyo', 'zankyo-wrapper'];
// /opt 直下の LICENSE は他の Layer と同名になりうるため、ディレクトリを分ける。
const NOTICE_DIR = 'share/licenses/zankyo';
const NOTICES = [`${NOTICE_DIR}/LICENSE`, `${NOTICE_DIR}/THIRD_PARTY_LICENSES`];
/// zip のエントリ。zip には引数の順で入る。
const ENTRIES = [...EXECUTABLES, ...NOTICES];

interface CargoPackage {
  id: string;
  name: string;
  version: string;
  license: string | null;
  license_file: string | null;
  repository: string | null;
  manifest_path: string;
}
interface CargoMetadata {
  packages: CargoPackage[];
  workspace_members: string[];
  resolve: {
    nodes: { id: string; deps: { pkg: string; dep_kinds: { kind: string | null }[] }[] }[];
  };
}

const LICENSE_FILE = /^(licen[cs]e|copying|copyright|notice)/i;
/// crates.io のパッケージにライセンスの本文が無い crate。本文は、そのパッケージを
/// 作った上流のコミット（Nugine/simd d74c030）の LICENSE を写した。
const LICENSE_FALLBACK: Record<string, string> = {
  'base64-simd': 'scripts/licenses/nugine-simd-LICENSE',
  vsimd: 'scripts/licenses/nugine-simd-LICENSE',
};

const byCodePoint = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

/**
 * バイナリに入る crate の、ライセンスと著作権表示の原文を 1 つのテキストにまとめる。
 * 対象は zankyo から normal 依存でたどれる crate（build-/dev-dependencies は配布物に入らない）。
 * 同じ本文は 1 回だけ載せ、見出しにその本文を持つ crate をすべて並べる。
 */
function thirdPartyLicenses(target: string): string {
  const meta: CargoMetadata = JSON.parse(
    execFileSync(
      'cargo',
      ['metadata', '--format-version', '1', '--locked', '--filter-platform', target],
      { cwd: ROOT, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 },
    ),
  );
  const root = meta.packages.find(
    (p) => p.name === 'zankyo' && meta.workspace_members.includes(p.id),
  );
  if (!root) throw new Error('cargo metadata: zankyo is not a workspace member');
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const reached = new Set<string>();
  const queue = [root.id];
  for (let id = queue.pop(); id !== undefined; id = queue.pop()) {
    for (const dep of nodes.get(id)?.deps ?? []) {
      if (!reached.has(dep.pkg) && dep.dep_kinds.some((k) => k.kind === null)) {
        reached.add(dep.pkg);
        queue.push(dep.pkg);
      }
    }
  }
  const crates = meta.packages
    .filter((p) => reached.has(p.id))
    .sort((a, b) => byCodePoint(a.name, b.name) || byCodePoint(a.version, b.version));

  const headingsByText = new Map<string, string[]>();
  for (const c of crates) {
    const dir = dirname(c.manifest_path);
    const files = new Set(
      readdirSync(dir)
        .filter((f) => LICENSE_FILE.test(f) && statSync(join(dir, f)).isFile())
        .sort(byCodePoint),
    );
    if (c.license_file) files.add(c.license_file);
    const sources = [...files].map((f) => ({ label: f, path: join(dir, f) }));
    if (sources.length === 0) {
      const fallback = LICENSE_FALLBACK[c.name];
      if (!fallback) {
        throw new Error(`${c.name} ${c.version} ships no license file; add it to LICENSE_FALLBACK`);
      }
      sources.push({ label: `LICENSE from ${c.repository}`, path: join(ROOT, fallback) });
    }
    for (const { label, path } of sources) {
      const text = readFileSync(path, 'utf8').replace(/\r\n?/g, '\n').trimEnd();
      const heading = `${c.name} ${c.version}: ${label}`;
      const headings = headingsByText.get(text);
      if (headings) headings.push(heading);
      else headingsByText.set(text, [heading]);
    }
  }

  const rule = '='.repeat(80);
  return [
    `Third-party licenses for zankyo ${root.version} (${target})`,
    '',
    'The zankyo binary in this layer is built from the Rust crates listed below.',
    'This file reproduces the license and notice files that each crate ships.',
    'A text shared by several crates appears once, under all of their names.',
    '',
    ...crates.map((c) => `  ${c.name} ${c.version} (${c.license ?? c.license_file})`),
    ...[...headingsByText].flatMap(([text, headings]) => ['', rule, ...headings, rule, '', text]),
    '',
  ].join('\n');
}

const arches = (process.env.ARCHES ?? 'x86_64 aarch64').split(/\s+/).filter(Boolean);
if (arches.length === 0) {
  console.error('ARCHES is empty; nothing to build');
  process.exit(1);
}
const outDirEnv = process.env.OUT_DIR ?? 'dist/layer';
const outDir = isAbsolute(outDirEnv) ? outDirEnv : resolve(ROOT, outDirEnv);
mkdirSync(outDir, { recursive: true });
mkdirSync(join(ROOT, 'sar/dist'), { recursive: true });

// BUILDER は `cargo build` 相当のコマンド行全体（"cross build" や
// "cargo zigbuild"）として解釈する。分割は空白区切りのみで
// シェル式のクォートは解釈しない。未指定なら cross があれば使い、
// 無ければ cargo に落ちる。
const builderArgv = () =>
  (
    process.env.BUILDER ??
    (spawnSync('cross', ['--version'], { stdio: 'ignore' }).status === 0
      ? 'cross build'
      : 'cargo build')
  )
    .split(/\s+/)
    .filter(Boolean);
const skipBuild = process.env.SKIP_BUILD === '1';
// 梱包専用（SKIP_BUILD=1）では builder に一切触れない。
const [builderCmd, ...builderArgs] = skipBuild ? [undefined] : builderArgv();
if (!skipBuild && !builderCmd) {
  console.error('BUILDER is empty');
  process.exit(1);
}
const EPOCH = new Date(0);

for (const arch of arches) {
  const target = TARGETS[arch];
  if (!target) {
    console.error(`unknown arch: ${arch}`);
    process.exit(1);
  }
  if (!skipBuild && builderCmd) {
    console.log(`== building ${target} ==`);
    execFileSync(
      builderCmd,
      [...builderArgs, '--locked', '--release', '--target', target, '-p', 'zankyo'],
      { cwd: ROOT, stdio: 'inherit' },
    );
  }

  const thirdParty = thirdPartyLicenses(target);
  const stage = mkdtempSync(join(tmpdir(), 'zankyo-layer-'));
  try {
    mkdirSync(join(stage, 'bin'), { recursive: true });
    mkdirSync(join(stage, 'extensions'), { recursive: true });
    mkdirSync(join(stage, NOTICE_DIR), { recursive: true });
    cpSync(join(ROOT, 'target', target, 'release', 'zankyo'), join(stage, 'bin/zankyo'));
    cpSync(join(ROOT, 'proxy/layer/zankyo-wrapper'), join(stage, 'zankyo-wrapper'));
    // /opt/extensions/zankyo: platform が external extension として
    // 別プロセス起動し、SHUTDOWN（timeout）イベントを届ける。
    cpSync(join(ROOT, 'proxy/layer/extensions/zankyo'), join(stage, 'extensions/zankyo'));
    cpSync(join(ROOT, 'LICENSE'), join(stage, NOTICE_DIR, 'LICENSE'));
    writeFileSync(join(stage, NOTICE_DIR, 'THIRD_PARTY_LICENSES'), thirdParty);
    for (const f of EXECUTABLES) {
      chmodSync(join(stage, f), 0o755);
    }
    for (const f of NOTICES) {
      chmodSync(join(stage, f), 0o644);
    }
    // 決定的 zip: mtime を epoch 固定（zip 側で 1980-01-01 に揃う）、
    // -X で拡張属性を捨てる。ディレクトリのエントリは作らず、ファイルを
    // 固定の引数順で渡すので、同一バイナリから同一 zip になる。
    for (const f of ENTRIES) {
      utimesSync(join(stage, f), EPOCH, EPOCH);
    }
    const zip = join(outDir, `zankyo-${arch}.zip`);
    // zip は既存アーカイブへ追記・更新するので、先に消して
    // 古いレイアウトのエントリが混入しないようにする。
    rmSync(zip, { force: true });
    execFileSync('zip', ['-qX', zip, ...ENTRIES], { cwd: stage });
    console.log(`wrote ${zip}`);

    // SAR テンプレートの ContentUri（sar/dist/layer-*.zip）に合わせて複写する。
    // sam publish はアプリケーションディレクトリ内のパスしか参照できないため。
    const sarZip = join(ROOT, 'sar/dist', `layer-${arch}.zip`);
    rmSync(sarZip, { force: true });
    cpSync(zip, sarZip);
    console.log(`wrote ${sarZip} (for sam publish)`);
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}
