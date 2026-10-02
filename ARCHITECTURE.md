# Architecture

## 全体像

```text
Lambda Runtime API (localhost)
        ▲   │  /next, /response, /error, /init/error
        │   ▼
┌─────────────────────────┐        ┌──────────────┐
│  zankyo proxy プロセス   │  PUT   │  S3 bucket   │
│  (exec wrapper 起動)    ├───────►│  zankyo/...  │
└─────────────────────────┘        └──────────────┘
        │ .inflight ステージ (/tmp)
        ▼
┌─────────────────────────┐
│  zankyo agent プロセス   │
│  (/opt/extensions 起動)  │
└─────────────────────────┘
        ▲   │  /extension/register, /event/next
        └───┘  Extensions API (SHUTDOWN 検知)
```

exec wrapper (`AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper`) が
zankyo バイナリを起動し、zankyo が実際の runtime を子プロセスとして
実行する。handler から見た Runtime API の振る舞いは変わらない。

timeout 捕捉は external extension が担う。internal extension
（exec wrapper 内からの register）には AWS が SHUTDOWN を
配信しないため、layer は `/opt/extensions/zankyo` を置き、
platform が別プロセスで agent を起動する。in-flight イベントは
プロセスをまたげないため、proxy が `/next` 時点で `/tmp` へ
`.inflight` ステージを書き、agent が SHUTDOWN で読んで
timeout レコードへ変換する。完了した呼び出しのステージは
proxy が即削除する（残存＝未完の証跡）。
agent は passthrough 判定でも終了せず、`SHUTDOWN` だけを購読して
待機する。登録前や `SHUTDOWN` 前に extension が終了すると、
終了コードに関係なく platform が Init を失敗させるため。
このとき設定が壊れている場合に備えて、Extensions API の設定
（`ZANKYO_REGISTER_TIMEOUT_MS`・`ZANKYO_EXT_RETRY_MS`・`ZANKYO_EXT_BODY_KB`・
`ZANKYO_EXT_MAX_POLL_FAILURES`）は読まず、既定値を使う。
同じ理由で、register と `/event/next` の一時的な失敗は再試行し、
`SHUTDOWN` まで終了しない。終了するのは、登録の 4xx 拒否・500
（公式リファレンスが「回復不能。速やかに終了」と定める）・
Runtime API への接続不能の継続（`ZANKYO_EXT_MAX_POLL_FAILURES` 回）の
ときだけで、終了コードは 1 にする。

## 品質・保守性の不変条件

このプロジェクトは次の不変条件を守る。レビュー・生成コードも同じ基準。

- **設定は既定値 → env → SSM overlay の順に重ねて解決**（後が優先）。
  例外は SSM を読む前に決まるもの。取得に使う `ZANKYO_SSM_PARAM`・
  `ZANKYO_SSM_TIMEOUT_MS` は env だけで決め、SSM の JSON では上書きしない
  （`ssm_overlay.rs` の `ENV_ONLY_KEYS`）。env の `ZANKYO_DISABLED` の真と
  env の設定エラーは、SSM を読まずに passthrough を決める。
- **利用者が調整する動作ノブ（上限・タイムアウト・間隔）をコードへ
  埋め込まない**。`ZANKYO_*` で変えられるようにし、新しい数値設定は
  `config.rs` の env 読み取り + `ssm_overlay.rs` の
  `SSM_NUM_FIELDS` に 1 行で済む。例外は下の「固定の安全上限」に挙げた
  定数だけで、足すときは名前付きの定数にして表にも載せる。
- **拡張はデータの 1 エントリ追加で完結**。scrub パターンは
  `scrub_data.rs`、CLI コマンドは `bin.ts` のディスパッチ、
  SSM キーは `SSM_NUM_FIELDS`、build arch は `TARGETS`。
- **エラーは握り潰さない**。失敗は `ZankyoError`/`Option` でモデル化し、
  捨てる経路はすべて文脈付きの warn で残す。
- **rerun は冪等**。spill は atomic 書き込み → 数・鮮度上限 →
  起動時・定期回収 → 送れたファイルの削除、とライフサイクルが閉じる。
  spill dir 自体は消さない（同時に走る書き込みが ENOENT で失敗するため）。
- **型は緩めない**。TS は strict + noUncheckedIndexedAccess、`as`
  キャストなし（AWS 境界は `ports.ts` の構造的ポート）。Rust は
  `#![forbid(unsafe_code)]` + clippy -D warnings。書式は
  biome / rustfmt で CI 強制。

## 固定の安全上限

次の値は設定で変えず、コードの定数に固定する。どれもプロトコルや実装の都合で
決まる安全のための上限か、短い待ち時間で、利用者の環境に合わせて変える値ではない。
変えられる値は README の設定表（`ZANKYO_*`）にある。`proxy/tests/docs_it.rs` が、
この表と `proxy/src` の定数を突き合わせる。

| 定数 | 値 | 場所 | 役割 |
|---|---|---|---|
| `EXT_BODY_TIMEOUT` | 1 秒 | `extension.rs` | Extensions API の応答ヘッダーを受けてから、ボディを読み切るまでの上限。登録では、残り時間の方が短ければ残り時間 |
| `ERROR_BODY_MAX_BYTES` | 4096 バイト | `extension.rs` | Extensions API のエラー応答のボディを、診断用に読む上限 |
| `SHUTDOWN_DEADLINE_MARGIN_MS` | 200 ms | `extension.rs` | SHUTDOWN の `deadlineMs` の手前に残す余白。フラッシュ予算は、余白を引いた残り時間と `ZANKYO_FLUSH_BUDGET_MS` の小さい方 |
| `SHUTDOWN_GRACE_CAP_MS` | 1 秒 | `orchestrate.rs` | 子が先に終了したとき、同じプロセスの extension に SHUTDOWN が届くのを待つ上限（`ZANKYO_FLUSH_BUDGET_MS` との小さい方） |
| `DRAIN_SLACK_MS` | 1 秒 | `orchestrate.rs` | 終了時に保存の完了を待つ時間枠で、`ZANKYO_PUT_TIMEOUT_MS` に足す余白 |
| `ACCEPT_BACKOFF` | 50 ms | `proxy.rs` | Runtime API の listen で、accept に失敗したときの再試行間隔 |
| `DRAIN_POLL` | 10 ms | `proxy.rs` | 終了時に、実行中のハンドラが無くなったかを確かめる間隔 |
| `PART_ORPHAN_GRACE` | 60 秒 | `spill.rs` | 書きかけの `.part` を、クラッシュ後の残りとみなして消すまでの猶予 |
| `RECORDED_CAP` | 4096 件 | `inflight.rs` | 記録済みの requestId（二重記録の防止用）を覚えておく件数の上限 |

設定で指定しても効かない場合が 1 つある。記録しない状態の agent は、設定が壊れている
場合に備えて、Extensions API の 4 つの設定を読まず、既定値（`config.rs` の定数）を使う
（上の「全体像」）。

## モジュール対応（proxy/src）

| ファイル | 責務 |
|---|---|
| `main.rs` | exec wrapper 起点。argv 解釈と logging 初期化のみ |
| `setup.rs` | 起動判定（env + SSM overlay → `StartupPlan`）と Recorder 構築 |
| `orchestrate.rs` | Record 確定後の配線。listen・子プロセス・extension・spill 回復を直線で起動 |
| `proxy.rs` | Runtime API の listen・経路判定・接続管理 |
| `handlers.rs` | `/next`・`/response`・`/error`・`/init/error` の個別処理 |
| `upstream.rs` | 上流 Runtime API への転送。hop-by-hop 除去と上限付きボディ読み |
| `extension.rs` | Extensions API。internal ループと external agent（`.inflight` 変換・passthrough 時の待機） |
| `inflight.rs` | `/next`〜確定までのイベント保持（Mutex<HashMap>） |
| `store.rs` | S3 PutObject とレコード組み立て（scrub 適用） |
| `spill.rs` | /tmp 退避の管理。書き込み・回収・保持数上限・廃棄 |
| `record.rs` | 保存レコードのスキーマ生成（serde） |
| `scrub.rs` / `scrub_data.rs` | PII マスキングの純粋ロジックと判定データ |
| `config.rs` / `ssm_overlay.rs` | env 設定解決と SSM JSON overlay（キーはテーブル駆動） |
| `runtime.rs` | 子プロセス起動（passthrough / proxy）と終了コード変換 |
| `ssm.rs` | SSM Parameter Store 取得（既定 2s timeout、fail-open） |
| `error.rs` / `lib.rs` | `ZankyoError` 統一型と公開面 |

## 拡張ポイント

- **scrub 対象**: `ZANKYO_SCRUB_FIELDS` に足す（コード不要）。
  パターン自体は `scrub_data.rs` に 1 エントリ。
- **設定項目**: `Config` + env 読み取りに追加。SSM は
  `SSM_NUM_FIELDS` に 1 行（フィールド単位パースで 1 キーの
  書き損じが全体を壊さない）。SSM を読む前に使う値は表に載せず、
  `ENV_ONLY_KEYS` に足す。
- **CLI コマンド**: `cli/src/commands/` に 1 ファイル + `bin.ts` に
  1 行。AWS 境界は `ports.ts` の構造的ポートに閉じる。
- **failure type**: `record.rs` enum・写像・CLI の
  `KNOWN_FAILURE_TYPES`・スキーマ doc。wire は opaque string で
  新旧 proxy/CLI 間でも壊れない（前方互換）。
- **build arch**: `build-layer.mts` の `TARGETS` + CI build +
  SAR テンプレートの Layer を対で追加。
- **テスト**: Rust は `proxy/tests/` が mock Runtime API + mock S3、
  CLI は `deps` 引数へ `tests/helpers.ts` のフェイクを差し込む。

## データフロー

1. `/next` 応答を中継しつつ requestId とイベントを `InFlight` に保持。
   同時に `.inflight` ステージを /tmp へ書く（agent との共有）。
2. `/response` に `errorType` があれば失敗。`/error`・`/init/error` は
   常に失敗。完了した呼び出しは `InFlight` とステージから除去する。
3. 失敗時: イベントを scrub → レコード JSON を生成 → /tmp へ先書き
   （write-ahead spill）→ bounded な S3 PutObject を **応答転送の前に**
   完了させる。呼び出し終了で環境が freeze されると非同期 PUT は
   進まないため、失敗経路だけこの順序を取る（成功経路は spill も PUT もしない）。
   PUT が間に合わなければ spill が残り、定期回収・次回 init が拾う。
4. SHUTDOWN（reason: timeout/failure/spindown）: external extension の
   agent が残った `.inflight` ステージを読み、「応答が返らなかった
   失敗」として timeout レコード化 → spill → 予算内で PUT。
   errorType は reason から写す。spill したレコードは次回起動時に
   `recover_spills` が S3 へ再送し、成功したものだけ削除する
   （冪等に再実行可能）。`.inflight` が init 時に残っていれば
   「応答なく畳まれた呼び出し」として同様に変換する。
