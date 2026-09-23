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
| `proxy.rs` | Runtime API の経路判定。イベント観測と失敗確定 |
| `upstream.rs` | 上流 Runtime API への転送。hop-by-hop 除去と上限付きボディ読み |
| `extension.rs` | Extensions API。SHUTDOWN で in-flight を flush（reason ごとに errorType を分ける） |
| `inflight.rs` | `/next`〜確定までのイベント保持（Mutex<HashMap>） |
| `store.rs` | S3 への PutObject。失敗時は spill_dir 退避＋起動時の再送＋保持数上限 |
| `record.rs` | 保存レコードのスキーマ生成（serde） |
| `scrub.rs` | PII マスキング。denylist + パターンの純粋ロジック |
| `scrub_data.rs` | scrub の判定データ。denylist と検出パターンをコードから分離し、追加はテーブルの 1 エントリで完結させる |
| `config.rs` | env / SSM JSON の設定解決。env 名はここに集約 |
| `runtime.rs` | 子プロセス起動（passthrough / proxy 経由）と終了コード変換 |
| `ssm.rs` | SSM Parameter Store 取得（10s timeout、fail-open） |
| `error.rs` | `ZankyoError` と `Result` の統一型 |
| `lib.rs` | 上記の公開。binary と integration test が共有する |

## 拡張ポイント

- **scrub 対象を増やす**: `ZANKYO_SCRUB_FIELDS` にフィールド名を足す
  （コード不要）。パターン自体を足す場合は `scrub_data.rs` の
  テーブルに 1 エントリ追加するだけ。
- **設定項目を増やす**: `config.rs` の `Config` と env 読み取り、
  SSM 用 `SsmConfig`（`ZANKYO_*` キー名で serde 対応済み）に 1 項目追加。
- **CLI コマンドを増やす**: `cli/src/commands/` に 1 ファイル追加し、
  `bin.ts` のディスパッチに 1 行登録。AWS 境界は `aws.ts` と
  `store.ts` / `invoke.ts` の薄い層に閉じているため、コマンド本体は
  純粋な変換ロジックとして書ける。
- **テスト**: Rust は `proxy/tests/` が mock Runtime API + mock S3 で
  実経路を検証。CLI はコマンドの `deps` 引数へ `tests/helpers.ts`
  のフェイクを差し込む（SDK 型への cast は helper 内に限定）。AWS 非依存。

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
