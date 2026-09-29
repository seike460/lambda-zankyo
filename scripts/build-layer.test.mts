//! build-layer.mts が、ビルドと梱包の前に止まる場合を確かめる。
//! rustc・cargo・cross は PATH の先頭に置いた偽物に差し替え、本物は起動しない。
//! 偽の cargo と cross は、起動されたことを記録して失敗する。

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(new URL('./build-layer.mts', import.meta.url));
const constant = (name: string) => {
  const value = new RegExp(`^const ${name} = '([^']+)';$`, 'm').exec(
    readFileSync(SCRIPT, 'utf8'),
  )?.[1];
  assert.ok(value, `${name} is not in build-layer.mts`);
  return value;
};
const RUST_VERSION = constant('RUST_VERSION');
const LIBRARY_NOTICE = fileURLToPath(
  new URL(`./licenses/rust-${RUST_VERSION}/COPYRIGHT-library.html`, import.meta.url),
);

const work = mkdtempSync(join(tmpdir(), 'build-layer-test-'));
after(() => rmSync(work, { recursive: true, force: true }));

interface Toolchain {
  release?: string;
  /// sysroot の share/doc/rust/COPYRIGHT-library.html。null なら置かない
  libraryNotice?: string | null;
}

/// 偽の rustc・cargo・cross を PATH に置いて build-layer.mts を実行する。
function run(env: Record<string, string>, toolchain: Toolchain = {}) {
  const dir = mkdtempSync(join(work, 'run-'));
  const bin = join(dir, 'bin');
  const sysroot = join(dir, 'sysroot');
  const started = join(dir, 'started');
  const out = join(dir, 'out');
  mkdirSync(bin);
  mkdirSync(join(sysroot, 'share/doc/rust'), { recursive: true });
  const notice = toolchain.libraryNotice;
  if (notice !== null) {
    writeFileSync(
      join(sysroot, 'share/doc/rust/COPYRIGHT-library.html'),
      notice ?? readFileSync(LIBRARY_NOTICE),
    );
  }
  const release = toolchain.release ?? RUST_VERSION;
  const tools: Record<string, string> = {
    rustc: [
      'case "$1" in',
      `  --version) echo 'rustc ${release} (fake)' ;;`,
      `  --print) echo '${sysroot}' ;;`,
      '  *) exit 2 ;;',
      'esac',
    ].join('\n'),
    cargo: `echo "cargo $*" >> '${started}'; exit 1`,
    cross: `echo "cross $*" >> '${started}'; exit 1`,
  };
  for (const [name, body] of Object.entries(tools)) {
    writeFileSync(join(bin, name), `#!/bin/sh\n${body}\n`);
    chmodSync(join(bin, name), 0o755);
  }
  // 呼び出し元の BUILDER や SKIP_BUILD は引き継がない
  const inherited = Object.fromEntries(
    Object.entries(process.env).filter(([key]) => !['BUILDER', 'SKIP_BUILD'].includes(key)),
  );
  const r = spawnSync(process.execPath, [SCRIPT], {
    env: {
      ...inherited,
      PATH: `${bin}:${process.env.PATH}`,
      ARCHES: 'x86_64',
      OUT_DIR: out,
      ...env,
    },
    encoding: 'utf8',
  });
  return {
    status: r.status,
    stderr: r.stderr,
    outDirCreated: existsSync(out),
    started: existsSync(started) ? readFileSync(started, 'utf8') : '',
  };
}

describe('build-layer.mts', () => {
  // THIRD_PARTY_LICENSES の musl と LLVM の表示は、rustc が自分の部品をリンクする
  // cross build と cargo build のもの。zig の libc と crt をリンクする cargo zigbuild は通さない
  it('refuses builders other than cross build and cargo build, before starting any tool', () => {
    const r = run({ BUILDER: 'cargo zigbuild' });
    assert.equal(r.status, 1);
    assert.match(
      r.stderr,
      /BUILDER must be one of: cross build, cargo build \(got "cargo zigbuild"\)/,
    );
    assert.equal(r.started, '');
    assert.equal(r.outDirCreated, false);
  });

  it('accepts cross build and cargo build', () => {
    for (const builder of ['cross build', ' cargo  build ']) {
      // 通った BUILDER は、rustc の版の検査まで進む（版が違うので、ビルドの前に止まる）
      const r = run({ BUILDER: builder }, { release: '0.0.0' });
      assert.equal(r.status, 1);
      assert.doesNotMatch(r.stderr, /BUILDER/);
      assert.match(
        r.stderr,
        new RegExp(`rustc 0\\.0\\.0 \\(fake\\) .*is not Rust ${RUST_VERSION}`),
      );
      assert.equal(r.started, '');
    }
  });

  it('stops when COPYRIGHT-library.html differs from the one shipped with rustc', () => {
    for (const libraryNotice of ['<html>another release</html>', null]) {
      const r = run({ SKIP_BUILD: '1' }, { libraryNotice });
      assert.equal(r.status, 1);
      assert.match(
        r.stderr,
        new RegExp(
          `scripts/licenses/rust-${RUST_VERSION}/COPYRIGHT-library\\.html is not the same`,
        ),
      );
      assert.equal(r.started, '');
      assert.equal(r.outDirCreated, false);
    }
  });
});
