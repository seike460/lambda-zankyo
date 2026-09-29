# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **proxy** — 宣言する MSRV（`rust-version`）を、ロック済み依存の実際の要件
  1.94.1 に合わせた。CI で宣言値のビルドを検証する
- **proxy** — 依存の hmac を 0.13 に、sha2 を 0.11 に、base64 を 0.23 に上げた
- **proxy** — denylist に一致した値が配列なら、要素ごとにマスクして配列の形を
  保つ。以前は配列全体を 1 つの文字列 `"***"` に置き換えていた
- **proxy** — `ZANKYO_SSM_TIMEOUT_MS` の既定値を 10000 から 2000 に下げた。
  SSM の取得は子ランタイムの起動と extension の登録より前に待つ。以前の既定値は
  Lambda の Init 上限（10 秒）と同じで、SSM に届かない VPC では Init が
  上限を超えうる
- **cli** — `fixture --out` は、新しく作るファイルを所有者だけが読める
  モード（0600）で書く。既存のファイルを上書きするときは、モードを変えない
- **cli** — npm の tarball から、使われない型定義（`.d.ts`）と source map を外した。
  source map は tarball に含まれない `src/` を指していた
- **sar** — SAR の `SourceCodeUrl` を、リポジトリのルートから、その版のタグ
  （`/tree/v<版>`）に変えた。SAR の各版から、対応するソースをたどれる
- **proxy** — `ZANKYO_EXT_MAX_POLL_FAILURES` は、Runtime API に接続できない状態が
  続いた回数だけを数える。応答が返る失敗（500 を除く）は数えず、`SHUTDOWN` まで
  再試行する。`ZANKYO_EXT_RETRY_MS` は、登録を再試行する間隔にも使う
- **layer** — `scripts/build-layer.mts` の `BUILDER` は、`cross build` と
  `cargo build` だけを受け付ける。`cargo zigbuild` などは、zig が持つ musl libc と
  CRT をリンクしうる。その場合、Layer のライセンス表示と中身が食い違う

### Fixed

- **proxy** — 記録しない状態（`ZANKYO_DISABLED`・バケット未設定・設定エラー）で、
  external extension が register せずに終了していた。Lambda はこれを
  Extension.Crash とみなし、関数の Init を失敗させうる。`SHUTDOWN` だけを
  購読して待機するようにした。このとき、設定が壊れている場合に備えて、
  `ZANKYO_REGISTER_TIMEOUT_MS`・`ZANKYO_EXT_RETRY_MS`・`ZANKYO_EXT_BODY_KB`・
  `ZANKYO_EXT_MAX_POLL_FAILURES` は読まず、既定値を使う
- **proxy** — external extension（agent）が、Extensions API の一時的な失敗で
  `SHUTDOWN` の前に終了していた。登録は 1 回の失敗で、`/event/next` は連続失敗が
  `ZANKYO_EXT_MAX_POLL_FAILURES` に達すると、終了コード 0 で終わっていた。
  Lambda はこれを Extension.Crash とみなし、Init の失敗や実行環境の再起動につながる。
  登録は `ZANKYO_REGISTER_TIMEOUT_MS` の時間内で再試行し、`/event/next` は
  `SHUTDOWN` まで再試行を続ける。終了するのは、登録の 4xx 拒否・Lambda の 500
  （AWS が回復不能と定める）・Runtime API への接続不能の継続のときだけで、
  終了コードは 1 にした
- **proxy** — SSM の JSON に書いた `ZANKYO_SSM_TIMEOUT_MS` を、警告なしに
  受け付けていたが、効いていなかった。SSM の取得の上限時間は、取得の前に
  環境変数の値で決まるため。このキーは `ZANKYO_SSM_PARAM` と同じく、環境変数で
  だけ指定できるものとした。SSM の JSON に書かれていたら、warn を出して無視する
- **proxy** — README と ARCHITECTURE.md は、設定の解決順を「env → SSM → 既定値」と
  説明していた。実際は既定値・環境変数・SSM の順に重ね、SSM の JSON にあるキーが
  優先する。説明を実際の動作に合わせ、SSM の値では変わらない場合（環境変数だけの
  キー・環境変数の `ZANKYO_DISABLED` の真・環境変数の設定エラー）も書いた。
  README の設定表には、SSM の取得に成功すれば、SSM の JSON の `ZANKYO_BUCKET` だけでも
  記録することを書いた
- **proxy** — README と ARCHITECTURE.md は、上限・タイムアウト・間隔をすべて
  `ZANKYO_*` で変えられると説明していた。実際は、Extensions API の応答ボディを
  読む上限時間（1 秒）など、設定で変えない安全のための上限がある。
  ARCHITECTURE.md に一覧（固定の安全上限）を置き、説明をこれに合わせた
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
- **proxy** — `ZANKYO_SCRUB_MODE=off` でも、`errorContext` の `errorMessage`・
  `stackTrace` などの自由テキストにはパターン検出のマスクが掛かっていた。
  説明どおり、off では自由テキストも原文のまま記録する
- **proxy** — 残った `.inflight` ステージを timeout レコードへ変換するとき、
  `/tmp` への spill と S3 への PUT がともに失敗しても、ステージを削除していた。
  このステージはレコードの最後のコピーで、記録が失われていた。どちらかが
  成功したときだけ削除し、両方失敗したら次の回収まで残す。読み取りに失敗した
  ステージも、削除せずに残す
- **proxy** — init 時の回収は、Runtime API の中継を始めた後に `.inflight`
  ステージを列挙していた。初回の `/next` が先に届くと、実行中の呼び出しを
  timeout として S3 に記録しうる。回収する一覧は、中継を始める前に確定する
- **proxy** — README とコードの説明では、ボディを読むのは `/next`・`/response`・
  `/error`・`/init/error` だけで、ほかの経路はそのまま中継するとしていた。実際は
  経路を問わず、リクエストボディを `ZANKYO_MAX_BODY_KB` まで読んでから中継し、
  超えたら中継せずに 413 を返す。説明を実際の動作に合わせた
- **proxy** — external extension（agent）が、Extensions API の応答ボディを
  時間の上限なしに読んでいた。ヘッダーだけ返してボディを閉じない応答で、登録は
  `ZANKYO_REGISTER_TIMEOUT_MS` を過ぎても戻らずに Init を遅らせ、`/event/next` は
  再試行も `SHUTDOWN` の検知もできなくなっていた。ボディの読み取りは、ヘッダーの後
  1 秒で打ち切る（登録では、残り時間がそれより短ければ残り時間で打ち切る）。
  打ち切った応答は、ほかの一時的な失敗と同じく再試行する
- **proxy** — `/next` の応答ボディを、時間の上限なしに読んでいた。Runtime API が
  ヘッダーだけ返してボディを閉じないと、ランタイムの `/next` が止まったままになる。
  ヘッダーの後のボディは `ZANKYO_FORWARD_TIMEOUT_MS` で打ち切り、ランタイムへ
  502 を返す。ヘッダーまでは従来どおり無制限に待つ（ロングポーリング）
- **cli** — replay・diff・redrive は、非 JSON イベント（`eventIsRawText` /
  `eventIsBase64`）のレコードを、invoke の前に exit 4 で止める。Lambda の
  Invoke API は JSON でない本文を `InvalidRequestContentException` で拒否する。
  以前は原文のまま送り、README もそれで再送できると説明していた。
  fixture は従来どおり、保存された形のまま書き出す
- **cli** — 数値の環境変数（`ZANKYO_LIST_PAGE_SIZE`・`ZANKYO_RECORD_MAX_MB` など）の
  小数を切り捨てていた。`0.5` は 0 になり、`MaxKeys: 0` の一覧や、上限 0 バイトの
  読み取りになっていた。正の整数でない値は、既定値を使う。タイムアウトの値が
  Node.js のタイマーの上限（2147483647 ms）を超える場合も、既定値を使う。以前は
  1 ms で打ち切られ、AWS API の呼び出しがすべて失敗していた
- **cli** — `list --since` で古いページがすべて除外されると、
  `ZANKYO_LIST_MAX_PAGES` を超えてバケットを最後まで走査していた。
  走査の上限を、除外後の件数ではなくページ数で数える
- **cli** — README の exit code の説明で、exit 4 を「レコード不在」だけとしていた。
  形式不正・サイズ超過（`ZANKYO_RECORD_MAX_MB` 超）・再現不能のレコードも exit 4 に
  なる。CLI の README からも、サイズ超過が抜けていた
- **cli** — S3 からレコードの本文を読む途中で通信が切れると、exit 2（引数・設定ミス）で
  終わっていた。getObject の失敗と同じく、AWS 側の失敗として exit 3 にする
- **cli / construct** — npm の tarball に LICENSE を同梱する。以前は
  MIT の許諾文が配布物に入っていなかった
- **layer** — Layer の zip に、LICENSE と `THIRD_PARTY_LICENSES` を同梱する
  （`/opt/share/licenses/zankyo/`）。以前は、MIT の許諾文も、バイナリに静的リンク
  した crate（tokio・hyper・aws-lc-rs・ring など）のライセンスと著作権表示も
  入っていなかった。`THIRD_PARTY_LICENSES` は、crate のほかに、musl ターゲットで
  静的リンクされる Rust 標準ライブラリ・musl libc・LLVM の libunwind と
  crtbegin/crtend の表示も含む。crate の表示には、サブディレクトリにあるもの
  （ring が取り込んだ once_cell と fiat-crypto、regex-syntax の Unicode の表など）も
  含む。Rust 標準ライブラリのファイルごとの表示は、
  Rust に同梱の `COPYRIGHT-library.html` を、同じディレクトリにそのまま置く
- **cli / construct** — pack と publish の前（`prepack`）に build を実行する。
  以前は dist が無いまま、または古いまま公開されうる手順だった
- **cli / construct** — package.json に repository・homepage・bugs・
  keywords・author を追加した。npm のページから GitHub へたどれる
- **construct** — 既定の SAR アプリケーション ID が、プレースホルダ
  （us-east-1 / アカウント 000000000000）のままだった。`layer` を渡さない構成は、
  存在しないアプリを参照してデプロイに失敗していた。公開中の
  `arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo`
  に直した。公開アプリなので、ほかのリージョンのスタックからもデプロイできる
- **construct** — `attachTo` は、SAR の Layer のアーキテクチャ（`arm64`）と関数の
  アーキテクチャが食い違うと例外を投げる。以前は synth が通り、関数は
  別アーキテクチャのバイナリを exec できずに起動しなくなっていた
- **construct** — `recordRetentionDays` が 1 以上の整数でなければ、synth で
  例外を投げる。以前は 0 が synth を通り、S3 のライフサイクル設定でデプロイが
  失敗していた
- **construct** — npm のページに使い方が何も表示されなかった。props と
  Layer の入手先を書いた README をパッケージに含める
- **docs** — README と SPEC の冒頭で、失敗した呼び出しを「確実に残す」と書いていた。
  記録はベストエフォートで、S3 に届かず `/tmp` にも残せなかった場合や、再送の前に
  実行環境が破棄された場合は失われる。表現を改め、README の制限事項に書き足した

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
- **construct** — `attachTo` が付ける `s3:PutObject` の対象を、`zankyo/*` から
  関数ごとの `zankyo/{関数名}/*` に絞った。以前は、バケットを共有する別の関数が、
  この関数を名乗るレコードを置けた。権限は role の DefaultPolicy ではなく、
  関数ごとの `AWS::IAM::Policy` に入る。既存のスタックを更新すると、デプロイ中の
  短い間だけ PUT が拒否されることがある。その間のレコードは spill に退避され、
  実行環境が残っていれば、定期回収か次回 init で再送される
- **cli** — S3 キーの関数名と本文の `functionName` が食い違うレコードを、
  exit 4 で拒否する。以前は本文の関数名をそのまま replay・diff・redrive の
  invoke 先にしていた。バケットに書ける別の関数が、運用者の権限で任意の関数へ
  ペイロードを投入させられた

## [0.1.0] - 2026-09-23

Initial release.

タグ `v0.1.0` は d220009 を指す。公開した SAR 0.1.0 のメタデータ（`LicenseUrl`・
`ReadmeUrl` のパス）は、タグの後のコミット 83e4929 の内容である。タグの
`sar/template.yaml` のままでは、この 2 つのパスがリポジトリの外を指す。83e4929 は
ほかに `construct/package.json` の `main`・`types` を dist に向けたが、npm の
`zankyo-cdk@0.1.0` はタグ時点の `publishConfig` で同じ値になっている。
proxy・CLI・construct のコードは、タグと 83e4929 で同じである。

### Added

- **proxy** — `AWS_LAMBDA_EXEC_WRAPPER` 経由で Runtime API を仲介し、失敗した
  同期呼び出しを S3 へ記録する単一静的バイナリ（musl、x86_64 / aarch64）
  - `handler_error` / `init_error` / `timeout` の 3 種を記録。成功呼び出しは記録しない
  - 失敗レコードは write-ahead spill（`/tmp`）→ 転送前の bounded PUT で
    freeze 負けを防ぎ、PUT 失敗時は次回 init・定期回収で再送
  - `/opt/extensions/zankyo` の external extension が `SHUTDOWN` を捕捉し、
    `/tmp` の `.inflight` ステージから timeout レコードを復元
  - イベント保持は UTF-8 安全（JSON は JSON、非 JSON は raw text、
    非 UTF-8 は base64）。フィールド名 denylist とパターン検出（email・
    カード番号（Luhn 検証付き）・JWT・AWS アクセスキー・Bearer トークン・
    電話番号・IPv4）で scrub
  - SSM Parameter Store の JSON overlay で設定上書き可能。fail-open 設計で
    zankyo 側の障害はハンドラを止めない
- **cli** (`lambda-zankyo`) — `list` / `fixture` / `replay` / `diff` / `redrive`
  コマンド。fixture 出力は `sam local invoke -e` にそのまま渡せる
- **construct** (`zankyo-cdk`) — `new Zankyo(this, 'Zankyo').attachTo(fn)` で SAR 参照・
  env 注入・IAM 権限を一括配線する CDK construct
- **sar/** — Serverless Application Repository 公開用 SAM テンプレート
  （x86_64 / arm64 の 2 Layer を 1 アプリとして公開）
- examples、CI（fmt/clippy/test/biome/typecheck/build）、決定的 zip 梱包

[Unreleased]: https://github.com/seike460/lambda-zankyo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/seike460/lambda-zankyo/releases/tag/v0.1.0
