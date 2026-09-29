//! build-layer.mts が、ビルドと梱包の前に止まる場合と、梱包（SKIP_BUILD=1）の中身を確かめる。
//! rustc・cargo・cross は PATH の先頭に置いた偽物に差し替え、本物は起動しない。
//! 偽の cargo と cross は、起動されたことを記録して失敗する（cargo metadata は除く）。

import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import {
  chmodSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('..', import.meta.url));
const SCRIPT = fileURLToPath(new URL('./build-layer.mts', import.meta.url));
const constant = (name: string) => {
  const value = new RegExp(`^const ${name} = '([^']+)';$`, 'm').exec(
    readFileSync(SCRIPT, 'utf8'),
  )?.[1];
  assert.ok(value, `${name} is not in build-layer.mts`);
  return value;
};
const RUST_VERSION = constant('RUST_VERSION');
const LLVM_VERSION = constant('LLVM_VERSION');
const LIBRARY_NOTICE = fileURLToPath(
  new URL(`./licenses/rust-${RUST_VERSION}/COPYRIGHT-library.html`, import.meta.url),
);

const work = mkdtempSync(join(tmpdir(), 'build-layer-test-'));
after(() => rmSync(work, { recursive: true, force: true }));

interface Toolchain {
  release?: string;
  llvm?: string;
  /// sysroot の share/doc/rust/COPYRIGHT-library.html。null なら置かない
  libraryNotice?: string | null;
  /// `cargo metadata` が出力する JSON のファイル。無ければ cargo metadata も失敗する
  metadata?: string;
}

/// 偽の rustc・cargo・cross を PATH に置いて build-layer.mts（既定はこのリポジトリのもの）を実行する。
function run(env: Record<string, string>, toolchain: Toolchain = {}, script = SCRIPT) {
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
      `  -vV) printf 'rustc %s (fake)\\nrelease: %s\\nLLVM version: %s\\n' '${release}' '${release}' '${toolchain.llvm ?? LLVM_VERSION}' ;;`,
      `  --print) echo '${sysroot}' ;;`,
      '  *) exit 2 ;;',
      'esac',
    ].join('\n'),
    cargo: [
      ...(toolchain.metadata ? [`[ "$1" = metadata ] && exec cat '${toolchain.metadata}'`] : []),
      `echo "cargo $*" >> '${started}'; exit 1`,
    ].join('\n'),
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
  const r = spawnSync(process.execPath, [script], {
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
    outDir: out,
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

  it('stops when rustc is not RUST_VERSION', () => {
    const r = run({ SKIP_BUILD: '1' }, { release: '0.0.0' });
    assert.equal(r.status, 1);
    assert.match(r.stderr, new RegExp(`is not Rust ${RUST_VERSION}`));
    assert.equal(r.started, '');
    assert.equal(r.outDirCreated, false);
  });

  it("stops when rustc's LLVM is not LLVM_VERSION", () => {
    const r = run({ SKIP_BUILD: '1' }, { llvm: '0.0.0' });
    assert.equal(r.status, 1);
    assert.ok(
      r.stderr.includes(
        `(LLVM 0.0.0) is not Rust ${RUST_VERSION} (LLVM ${LLVM_VERSION}); update TOOLCHAIN_NOTICES`,
      ),
      r.stderr,
    );
    assert.equal(r.started, '');
    assert.equal(r.outDirCreated, false);
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

describe('build-layer.mts packaging (SKIP_BUILD=1)', () => {
  it('packs COPYRIGHT-library.html, and the license files in the subdirectories of a crate', () => {
    // 梱包に要るファイルだけを持つリポジトリの写しと、ビルド済みの体のバイナリを作る
    const root = mkdtempSync(join(work, 'repo-'));
    for (const path of ['scripts/build-layer.mts', 'scripts/licenses', 'LICENSE', 'proxy/layer']) {
      cpSync(join(REPO, path), join(root, path), { recursive: true });
    }
    const binary = join(root, 'target/x86_64-unknown-linux-musl/release/zankyo');
    mkdirSync(dirname(binary), { recursive: true });
    writeFileSync(binary, 'zankyo');
    // 別のプロジェクトのコードを取り込み、その表示をサブディレクトリに持つ crate
    const crate = join(root, 'registry/vendoring-1.0.0');
    const files: Record<string, string> = {
      'LICENSE-MIT': 'vendoring: its own license',
      'src/vendored/LICENSE': 'vendored code: the license of the project it came from',
      'tests/data/LICENSE': 'test data: not built into the binary',
    };
    for (const [path, text] of Object.entries(files)) {
      mkdirSync(dirname(join(crate, path)), { recursive: true });
      writeFileSync(join(crate, path), text);
    }
    const zankyo = 'path+file:///zankyo#0.1.0';
    const vendoring = 'registry+https://github.com/rust-lang/crates.io-index#vendoring@1.0.0';
    const pkg = (id: string, name: string, dir: string) => ({
      id,
      name,
      version: id.endsWith('1.0.0') ? '1.0.0' : '0.1.0',
      license: 'MIT',
      license_file: null,
      repository: null,
      manifest_path: join(dir, 'Cargo.toml'),
    });
    const metadata = join(root, 'metadata.json');
    writeFileSync(
      metadata,
      JSON.stringify({
        packages: [pkg(zankyo, 'zankyo', join(root, 'proxy')), pkg(vendoring, 'vendoring', crate)],
        workspace_members: [zankyo],
        resolve: {
          nodes: [
            { id: zankyo, deps: [{ pkg: vendoring, dep_kinds: [{ kind: null }] }] },
            { id: vendoring, deps: [] },
          ],
        },
      }),
    );

    const r = run({ SKIP_BUILD: '1' }, { metadata }, join(root, 'scripts/build-layer.mts'));
    assert.equal(r.status, 0, r.stderr);
    const zip = join(r.outDir, 'zankyo-x86_64.zip');
    const entry = (path: string) =>
      execFileSync('unzip', ['-p', zip, path], { maxBuffer: 64 * 1024 * 1024 });
    assert.deepEqual(execFileSync('unzip', ['-Z1', zip], { encoding: 'utf8' }).split('\n'), [
      'bin/zankyo',
      'extensions/zankyo',
      'zankyo-wrapper',
      'share/licenses/zankyo/LICENSE',
      'share/licenses/zankyo/THIRD_PARTY_LICENSES',
      'share/licenses/zankyo/COPYRIGHT-library.html',
      '',
    ]);
    assert.ok(
      entry('share/licenses/zankyo/COPYRIGHT-library.html').equals(readFileSync(LIBRARY_NOTICE)),
    );
    const thirdParty = entry('share/licenses/zankyo/THIRD_PARTY_LICENSES').toString('utf8');
    assert.match(thirdParty, /^vendoring 1\.0\.0: LICENSE-MIT\n=+\n\nvendoring: its own license$/m);
    assert.match(
      thirdParty,
      /^vendoring 1\.0\.0: src\/vendored\/LICENSE\n=+\n\nvendored code: the license of the project it came from$/m,
    );
    assert.doesNotMatch(thirdParty, /test data/);
  });
});
