#!/usr/bin/env bash
# zankyo Layer zip のビルド。x86_64 / aarch64 の musl 静的バイナリを作り、
# Lambda が /opt に展開するレイアウト（bin/zankyo + zankyo-wrapper）で zip 化する。
#
# 前提: cross (https://github.com/cross-rs/cross) か musl-gcc ツールチェーン。
# macOS では `brew install cargo-zigbuild` + cargo zigbuild でも代替できる。
set -euo pipefail

cd "$(dirname "$0")/.."

ARCHES="${ARCHES:-x86_64 aarch64}"
OUT_DIR="${OUT_DIR:-dist/layer}"
mkdir -p "$OUT_DIR"

for arch in $ARCHES; do
  case "$arch" in
    x86_64) target="x86_64-unknown-linux-musl" ;;
    aarch64) target="aarch64-unknown-linux-musl" ;;
    *) echo "unknown arch: $arch" >&2; exit 1 ;;
  esac

  echo "== building $target =="
  if command -v cross >/dev/null 2>&1; then
    cross build --release --target "$target" -p zankyo
  else
    cargo build --release --target "$target" -p zankyo
  fi

  stage="$(mktemp -d)"
  mkdir -p "$stage/bin"
  cp "target/$target/release/zankyo" "$stage/bin/zankyo"
  cp proxy/layer/zankyo-wrapper "$stage/zankyo-wrapper"
  chmod +x "$stage/bin/zankyo" "$stage/zankyo-wrapper"
  # 決定的 zip: mtime を epoch 固定、-X で拡張属性を捨て、
  # エントリ順を sort で安定化する（同一バイナリなら同一 zip になる）
  find "$stage" -exec touch -t 197001010000.00 {} +
  (cd "$stage" && find . -type f -print | LC_ALL=C sort | zip -qX "$OLDPWD/$OUT_DIR/zankyo-$arch.zip" -@)
  rm -rf "$stage"
  echo "wrote $OUT_DIR/zankyo-$arch.zip"

  # SAR テンプレートの ContentUri（sar/dist/layer-*.zip）に合わせて複写する。
  # sam publish はアプリケーションディレクトリ内のパスしか参照できないため。
  mkdir -p sar/dist
  cp "$OUT_DIR/zankyo-$arch.zip" "sar/dist/layer-$arch.zip"
  echo "wrote sar/dist/layer-$arch.zip (for sam publish)"
done
