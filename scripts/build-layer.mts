#!/usr/bin/env node
//! zankyo Layer zip のビルド。x86_64 / aarch64 の musl 静的バイナリを作り、
//! Lambda が /opt に展開するレイアウト（bin/zankyo + zankyo-wrapper）で zip 化する。
//!
//! 前提: cross (https://github.com/cross-rs/cross) か musl ツールチェーン。
//! `BUILDER="cargo zigbuild"` のように `cargo build` 相当の
//! コマンド行を差し替えられる。
//! `SKIP_BUILD=1` でビルドを省き target/ 済みのバイナリだけ梱包する。
//! Node 24+ は型注釈を strip してそのまま実行する（ビルド不要）。

import { execFileSync, spawnSync } from 'node:child_process';
import { chmodSync, cpSync, mkdirSync, mkdtempSync, rmSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

/// arch 名 → Rust target triple の写像。新しい arch はここに 1 行足す。
const TARGETS: Record<string, string> = {
  x86_64: 'x86_64-unknown-linux-musl',
  aarch64: 'aarch64-unknown-linux-musl',
};

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
// "cargo zigbuild"）として解釈する。未指定なら cross があれば使い、
// 無ければ cargo に落ちる。
const builderArgv = (
  process.env.BUILDER ??
  (spawnSync('cross', ['--version'], { stdio: 'ignore' }).status === 0
    ? 'cross build'
    : 'cargo build')
)
  .split(/\s+/)
  .filter(Boolean);
const [builderCmd, ...builderArgs] = builderArgv;
if (!builderCmd) {
  console.error('BUILDER is empty');
  process.exit(1);
}
const skipBuild = process.env.SKIP_BUILD === '1';
const EPOCH = new Date(0);

for (const arch of arches) {
  const target = TARGETS[arch];
  if (!target) {
    console.error(`unknown arch: ${arch}`);
    process.exit(1);
  }
  if (!skipBuild) {
    console.log(`== building ${target} ==`);
    execFileSync(
      builderCmd,
      [...builderArgs, '--release', '--target', target, '-p', 'zankyo'],
      { cwd: ROOT, stdio: 'inherit' },
    );
  }

  const stage = mkdtempSync(join(tmpdir(), 'zankyo-layer-'));
  try {
    mkdirSync(join(stage, 'bin'), { recursive: true });
    cpSync(join(ROOT, 'target', target, 'release', 'zankyo'), join(stage, 'bin/zankyo'));
    cpSync(join(ROOT, 'proxy/layer/zankyo-wrapper'), join(stage, 'zankyo-wrapper'));
    chmodSync(join(stage, 'bin/zankyo'), 0o755);
    chmodSync(join(stage, 'zankyo-wrapper'), 0o755);
    // 決定的 zip: mtime を epoch 固定（zip 側で 1980-01-01 に揃う）、
    // -X で拡張属性を捨てる。エントリは引数順なので固定すれば
    // 同一バイナリから同一 zip になる。
    for (const f of ['bin', 'bin/zankyo', 'zankyo-wrapper']) {
      utimesSync(join(stage, f), EPOCH, EPOCH);
    }
    const zip = join(outDir, `zankyo-${arch}.zip`);
    // zip は既存アーカイブへ追記・更新するので、先に消して
    // 古いレイアウトのエントリが混入しないようにする。
    rmSync(zip, { force: true });
    execFileSync('zip', ['-qX', zip, 'bin/zankyo', 'zankyo-wrapper'], { cwd: stage });
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
