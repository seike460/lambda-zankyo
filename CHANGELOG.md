# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **proxy** — 宣言する MSRV（`rust-version`）を、ロック済み依存の実際の要件
  1.94.1 に合わせた。CI で宣言値のビルドを検証する
- **proxy** — denylist に一致した値が配列なら、要素ごとにマスクして配列の形を
  保つ。以前は配列全体を 1 つの文字列 `"***"` に置き換えていた
- **proxy** — `ZANKYO_SSM_TIMEOUT_MS` の既定値を 10000 から 2000 に下げた。
  SSM の取得は子ランタイムの起動と extension の登録より前に待つ。以前の既定値は
  Lambda の Init 上限（10 秒）と同じで、SSM に届かない VPC では Init が
  上限を超えうる

### Fixed

- **proxy** — 記録しない状態（`ZANKYO_DISABLED`・バケット未設定・設定エラー）で、
  external extension が register せずに終了していた。Lambda はこれを
  Extension.Crash とみなし、関数の Init を失敗させうる。`SHUTDOWN` だけを
  購読して待機するようにした
- **proxy** — 空文字の `ZANKYO_SSM_PARAM` を未設定として扱う。以前は init の
  たびに空の名前で SSM を呼び、起動を遅らせていた
- **proxy** — external extension 構成で、呼び出し中にランタイムがクラッシュすると、
  wrapper の終了が毎回約 1 秒遅れていた
- **proxy** — 成功呼び出しを含む Runtime API への全リクエストで、info レベルの
  ログを出していた。debug レベルに下げ、既定では出さない
- **proxy** — Node.js ランタイムはスタックトレースを `trace` キーで送るため、
  `errorContext.stackTrace` が空だった。`stackTrace` が無いときは `trace` を読む
- **proxy** — spill の回収が、空になった spill dir を削除していた。同時に走る
  inflight ステージや spill の書き込みが、まれに ENOENT で失敗していた。
  dir は削除せずに残す
- **proxy** — README と設定の説明で、`ZANKYO_FLUSH_BUDGET_MS` と
  `ZANKYO_PUT_TIMEOUT_MS` の役割を実際の動作に合わせた。失敗レコードの PUT
  （呼び出し中と SHUTDOWN 後）は前者、spill 再送の PUT は後者で打ち切る

### Security

- **proxy** — AWS SDK の既定 `rustls` feature が引き込む旧 TLS コネクタ
  （hyper 0.14 + rustls 0.21 + h2 0.3）を依存から外した。
  RUSTSEC-2026-0098 / RUSTSEC-2026-0099 / RUSTSEC-2026-0104 /
  RUSTSEC-2026-0258 の対象。実行時の HTTPS client は従来どおり
  rustls-aws-lc（hyper 1.x）
- **proxy** — 既定の scrub denylist に `cookie` と `cookies` を追加した。
  API Gateway・ALB・Function URL のイベントにある `Cookie` ヘッダ、
  HTTP API v2 の `cookies` 配列、応答の `Set-Cookie` が平文で残っていた
- **proxy** — JSON を文字列化した値（API Gateway・Function URL の `body` 等）の
  中のフィールドにも denylist を適用する。以前は `password` などがマスクされずに
  残っていた。一致した場合、その文字列は空白なし・キーは辞書順の JSON に書き直す

## [0.1.0] - 2026-09-23

Initial release.

### Added

- **proxy** — `AWS_LAMBDA_EXEC_WRAPPER` 経由で Runtime API を仲介し、失敗した
  同期呼び出しを S3 へ記録する単一静的バイナリ（musl、x86_64 / aarch64）
  - `handler_error` / `init_error` / `timeout` の 3 種を記録。成功呼び出しは記録しない
  - 失敗レコードは write-ahead spill（`/tmp`）→ 転送前の bounded PUT で
    freeze 負けを防ぎ、PUT 失敗時は次回 init・定期回収で再送
  - `/opt/extensions/zankyo` の external extension が `SHUTDOWN` を捕捉し、
    `/tmp` の `.inflight` ステージから timeout レコードを復元
  - イベント保持は UTF-8 安全（JSON は JSON、非 JSON は raw text、
    非 UTF-8 は base64）。denylist フィールドとカード番号パターンを scrub
  - SSM Parameter Store の JSON overlay で設定上書き可能。fail-open 設計で
    zankyo 側の障害はハンドラを止めない
- **cli** (`lambda-zankyo`) — `list` / `show` / `diff` / `fixture` / `invoke` /
  `store` コマンド。fixture 出力は `sam local invoke -e` にそのまま渡せる
- **construct** (`zankyo-cdk`) — `new Zankyo(stack).attachTo(fn)` で SAR 参照・
  env 注入・IAM 権限を一括配線する CDK construct
- **sar/** — Serverless Application Repository 公開用 SAM テンプレート
  （x86_64 / arm64 の 2 Layer を 1 アプリとして公開）
- examples、CI（fmt/clippy/test/biome/typecheck/build）、決定的 zip 梱包

[Unreleased]: https://github.com/seike460/lambda-zankyo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/seike460/lambda-zankyo/releases/tag/v0.1.0
