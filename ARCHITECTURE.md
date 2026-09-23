# Architecture

## 全体像

```text
Lambda Runtime API (localhost)
        ▲   │  /next, /response, /error, /init/error
        │   ▼
┌─────────────────────────┐        ┌──────────────┐
│  zankyo (proxy/src)     │  PUT   │  S3 bucket   │
│  Runtime API proxy      ├───────►│  zankyo/...  │
│  + external extension   │        └──────────────┘
└─────────────────────────┘
        ▲   │  /extension/register, /event/next
        └───┘  Extensions API (SHUTDOWN 検知)
```

exec wrapper (`AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper`) が
zankyo バイナリを起動し、zankyo が実際の runtime を子プロセスとして
実行する。handler から見た Runtime API の振る舞いは変わらない。

## モジュール対応（proxy/src）

| ファイル | 責務 |
|---|---|
| `main.rs` | exec wrapper 起点。argv 解釈と logging 初期化のみ |
| `setup.rs` | 起動判定（env + SSM overlay → `StartupPlan`）と Recorder 構築 |
| `orchestrate.rs` | Record 確定後の配線。listen・子プロセス・extension・spill 回復を直線で起動 |
| `proxy.rs` | Runtime API の listen・経路判定・接続管理 |
| `handlers.rs` | `/next`・`/response`・`/error`・`/init/error` の個別処理。イベント観測と失敗記録の起動 |
| `upstream.rs` | 上流 Runtime API への転送。hop-by-hop 除去と上限付きボディ読み |
| `extension.rs` | Extensions API。SHUTDOWN で in-flight を flush（reason ごとに errorType を分ける） |
| `inflight.rs` | `/next`〜確定までのイベント保持（Mutex<HashMap>） |
| `store.rs` | S3 への PutObject とレコード組み立て（scrub 適用） |
| `spill.rs` | /tmp 退避の管理。書き込み・回収・保持数上限・復旧不能ファイルの廃棄 |
| `record.rs` | 保存レコードのスキーマ生成（serde） |
| `scrub.rs` | PII マスキング。denylist + パターンの純粋ロジック |
| `scrub_data.rs` | scrub の判定データ。denylist と検出パターンをコードから分離し、追加はテーブルの 1 エントリで完結させる |
| `config.rs` | env 由来の設定解決と既定値。env 名はここに集約 |
| `ssm_overlay.rs` | SSM JSON overlay。数値キーはテーブル駆動でキー追加は 1 行 |
| `runtime.rs` | 子プロセス起動（passthrough / proxy 経由）と終了コード変換 |
| `ssm.rs` | SSM Parameter Store 取得（10s timeout、fail-open） |
| `error.rs` | `ZankyoError` と `Result` の統一型 |
| `lib.rs` | 上記の公開。binary と integration test が共有する |

## 拡張ポイント

- **scrub 対象を増やす**: `ZANKYO_SCRUB_FIELDS` にフィールド名を足す
  （コード不要）。パターン自体を足す場合は `scrub_data.rs` の
  テーブルに 1 エントリ追加するだけ。
- **設定項目を増やす**: `config.rs` の `Config` と env 読み取りに
  追加。SSM overlay 側は数値キーなら `ssm_overlay.rs` の
  `SSM_NUM_FIELDS` に 1 行、それ以外は match に 1 アーム追加
  （フィールド単位でパースするため、
  1 キーの書き損じで設定全体が捨てられない）。
- **CLI コマンドを増やす**: `cli/src/commands/` に 1 ファイル追加し、
  `bin.ts` のディスパッチに 1 行登録。AWS 境界は `ports.ts` の
  構造的ポート（`RecordReader`/`FunctionInvoker`）に閉じ、SDK 適合は
  `aws.ts` のアダプタだけが担うため、コマンド本体は純粋な変換
  ロジックとして書ける。
- **failure type を増やす**: `record.rs` の `FailureType` enum と
  シリアライズ値、shutdown reason → errorType の写像、CLI 側の
  `failureType` 検証リスト、レコードスキーマの doc に追加。
  wire 上の値は S3 レイアウトと後方互換を保つこと。
- **build arch を増やす**: `scripts/build-layer.mts` の `TARGETS` に
  1 行追加し、CI の build ステップと SAR テンプレートの Layer
  リソースを対に増やす。
- **テスト**: Rust は `proxy/tests/` が mock Runtime API + mock S3 で
  実経路を検証。CLI はコマンドの `deps` 引数へ `tests/helpers.ts`
  のフェイクを差し込む（ポートを直接実装するため cast 不要）。
  AWS 非依存。

## 品質・保守性の不変条件

このプロジェクトは次の不変条件を守る。レビュー・生成コードも同じ基準。

- **設定はすべて env → SSM overlay → 既定値の順で解決する**。動作ノブ
  （上限・タイムアウト・間隔）をコードへ埋め込まない。新しい数値設定は
  `config.rs` の env 読み取り + `ssm_overlay.rs` の `SSM_NUM_FIELDS`
  に 1 行で済む。
- **拡張はデータの 1 エントリ追加で完結させる**。scrub パターンは
  `scrub_data.rs`、CLI コマンドは `bin.ts` のディスパッチ、
  SSM キーは `SSM_NUM_FIELDS`、build arch は `TARGETS`。
- **エラーは握り潰さない**。失敗は `ZankyoError` または `Option` の
  戻り値でモデル化し、捨てる経路はすべて文脈付きの warn で残す。
- **rerun は冪等**。spill は atomic 書き込み → 数・鮮度上限 → 起動時・
  定期回収 → 全件回収で dir 自体削除、とライフサイクルが閉じており、
  再実行で残滓が増えない。
- **型は緩めない**。TS は strict + noUncheckedIndexedAccess、`as`
  キャストは使わない（AWS 境界は `ports.ts` の構造的ポート）。
  Rust は `#![forbid(unsafe_code)]` + clippy -D warnings。
  書式は biome / rustfmt で CI 強制する。

## データフロー

1. `/next` 応答を中継しつつ requestId とイベントを `InFlight` に保持。
2. `/response` に `errorType` があれば失敗。`/error`・`/init/error` は
   常に失敗。成功なら `InFlight` から除去して終わり。
3. 失敗時: イベントを scrub → レコード JSON を生成 →
   `s3://{ZANKYO_BUCKET}/zankyo/{function}/{yyyy}/{mm}/{dd}/{requestId}.json`
   に PutObject。
4. SHUTDOWN（reason: timeout/failure/spindown）: `InFlight` を drain
   して残りを「応答が返らなかった失敗」として flush。errorType は
   reason から写す。残り時間内に終わらなければ `/tmp/zankyo` に
   退避（取りこぼしうる、既知制約）。spill したレコードは次回起動時に
   `recover_spills` が S3 へ再送し、成功したものだけ削除する
   （冪等に再実行可能）。
