# lambda-zankyo（残響）

**sync invoke で失敗した Lambda 呼び出しの「イベント＋エラー応答」を確実に残し、
ローカル再現・差分リプレイ・本番再実行まで担う Layer＋CLI。**

> **スコープ**: 対象は **sync（RequestResponse）呼び出し** のみ。
> async invoke / stream invoke の失敗は **Lambda Destinations** に任せます
> （そちらは公式経路が存在します。zankyo は「無い方」を埋めるツールです）。

## なぜ存在するか

- Lambda Destinations は **async / stream 限定**。API Gateway・ALB・
  Function URL・直接 invoke・Cognito など **sync invoke の失敗イベントを
  保存する公式経路はありません**。
- sync の失敗は「エラーはログに残るが、**入力イベントは蒸発する**」。
  handler が event をログしていなければ再現不能です。
- Powertools `logEvent` や Middy はコード変更前提。Datadog は商用で
  fixture 出力・`sam local` 連携・差分リプレイがありません。

## アーキテクチャ

```
Lambda Service ──Runtime API──▶ zankyo proxy (Rust, Layer) ──▶ 実ランタイム(handler)
                     ▲                │        └─ /tmp に .inflight ステージ共有
                     │                ├─ 失敗時のみ: イベント+エラー → scrub → S3(+KMS)
                     │                └─ zankyo agent (/opt/extensions 起動)
                     │                     └─ Extensions API /register（SHUTDOWN 受信用）
                     └── AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper で差し込み
```

- Layer 内の**単一 Rust バイナリ**が Runtime API proxy と external extension
  agent を兼務（起動方法でモードが分かれる）。
- **成功呼び出しは記録しません**。S3 への PUT も spill もしません。ボディを読むのは
  `/next`・`/response`・`/error`・`/init/error` だけで、ほかの経路はそのまま中継します。
  `/next` のイベントは timeout 捕捉のため `/tmp` に置き、呼び出しが完了したら消します
  （下の「timeout 捕捉」）。
- 失敗の定義: `/error` 呼出 / `/init/error` / `/response` 内の `errorType`
  含有 / `SHUTDOWN` の時点で応答を返していない呼び出し（reason を問わず timeout として記録）。
- **fail-open 設計**: zankyo 側の障害（設定ミス・bind 失敗・S3 エラー）で
  関数本体を止めません。記録だけ諦めて passthrough します。

### timeout 捕捉

`SHUTDOWN` イベントは external extension にしか届かない AWS 仕様のため、
layer は `/opt/extensions/zankyo` を配置し、platform が agent を
別プロセスで起動します。proxy は呼び出し中のイベントを
`/tmp/zankyo/<function>/zankyo-{requestId}.inflight` にステージし、
完了時に削除します。agent は `SHUTDOWN` 受信時に残ったステージを
timeout レコードへ変換し S3 へフラッシュします（ベストエフォート。
shutdown ウィンドウに間に合わなければ spill が残り、次回 init が拾います）。

`SHUTDOWN` の reason（`timeout`・`failure`・`spindown`）は問いません。どの reason でも
`failureType` は `timeout` で、reason は `errorContext.errorType`（`Timeout`・`Failure`・
`Spindown`）で見分けます。呼び出し中にランタイムが終了した場合と、init 時に前の実行環境の
ステージが残っていた場合も timeout として記録し、`errorType` は `Shutdown` です。

記録しない状態（`ZANKYO_DISABLED`・バケット未設定・設定エラー）でも、
agent は `SHUTDOWN` だけを購読して待機します。登録前や `SHUTDOWN` 前に
終了した extension は、終了コードに関係なく Lambda が Init 失敗として
扱うためです。

## 使い方

### 1. Layer の導入

- **SAR（推奨）**: Serverless Application Repository から `lambda-zankyo`
  を 1 クリック導入（`sar/template.yaml` 参照）。アプリケーション ID は
  `arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo`
  です。公開アプリなので、ap-northeast-1 以外のリージョンにもデプロイできます
  （[AWS ドキュメント](https://docs.aws.amazon.com/serverlessrepo/latest/devguide/serverlessrepo-publishing-applications.html)）。
- **セルフホスト**: `node scripts/build-layer.mts` で両 arch の zip を作り、
  通常の Lambda Layer として発行します（GitHub Releases にも zip を添付）。
  v0.1.0 より後のリリースには、zip の `SHA256SUMS` も添付します。

いずれも関数に Layer を付け、環境変数を設定します:

```bash
AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper
ZANKYO_BUCKET=<記録用バケット名>
```

### 2. CDK（一番簡単）

```ts
import { Zankyo } from 'zankyo-cdk';

const zankyo = new Zankyo(this, 'Zankyo', {
  // bucket?, recordRetentionDays?, kmsKey?, scrubFields?,
  // sarApplicationId?, semanticVersion?, layer? (セルフホスト時), arm64?
});
zankyo.attachTo(myFunction);
// → Layer 追加 + AWS_LAMBDA_EXEC_WRAPPER/ZANKYO_* env 注入
//   + s3:PutObject（+ kms:Encrypt/GenerateDataKey）権限を role へ
```

bucket 未指定なら「パブリックアクセス全ブロック + lifecycle 30 日
（`recordRetentionDays` で変更）+ enforceSSL」のバケットを自動作成します。
このバケットは `RemovalPolicy.RETAIN` で、`cdk destroy` でスタックを消しても
レコードごと残ります。不要になったら、中身を消してからバケットを削除してください。
layer 未指定なら、上の SAR アプリを参照します。`arm64` は関数のアーキテクチャに
揃えてください。食い違うと `attachTo` が例外を投げます
（props の一覧は [construct/README.md](https://github.com/seike460/lambda-zankyo/blob/main/construct/README.md)）。

### 3. CLI

```bash
npm install -g lambda-zankyo   # npx lambda-zankyo でも可
export ZANKYO_BUCKET=my-records

zankyo list --function my-api --since 24h        # 失敗レコード一覧
zankyo fixture --last --out event.json           # sam local invoke -e 用
zankyo replay <requestId> --alias dev            # 指定バージョンで再実行
zankyo diff <requestId> --alias v12 --alias v13  # 新旧比較（CI 向け exit code）
zankyo redrive <requestId> --confirm             # 本番再投入（既定 dry-run）
```

共通フラグ: `--bucket` / `--region` / `--profile` / `--json`。
exit code: `0` 成功 / `1` diff差異・関数エラー / `2` 引数ミス / `3` AWS 失敗 / `4` レコード不在。

## 設定（環境変数）

`ZANKYO_SSM_PARAM` 指定時は SSM Parameter の JSON（同じキー名）が
env を部分上書きします。複数関数で設定を一元管理するための経路です。
このとき関数のロールに `ssm:GetParameter` が要ります。カスタマー管理キーで暗号化した
SecureString なら、そのキーの `kms:Decrypt` も要ります
（[AWS ドキュメント](https://docs.aws.amazon.com/kms/latest/developerguide/services-parameter-store.html#parameter-policy-kms-encryption)）。
CDK construct はこの権限を付けないので、別に付与してください。取得に失敗すると、warn を
出して env の設定だけで動きます（バケットを SSM 側にだけ書いた場合は、記録しません）。

| env | 既定 | 用途 |
|---|---|---|
| `ZANKYO_BUCKET` | （必須※） | 失敗レコードの保存先。未設定なら記録せず passthrough |
| `ZANKYO_KMS_KEY` | SSE-S3 | SSE-KMS のキー ARN |
| `ZANKYO_SSM_PARAM` | なし | 設定 JSON を保持する SSM Parameter 名 |
| `ZANKYO_SCRUB_FIELDS` | 既定 denylist | 追加フィールド名（カンマ区切り） |
| `ZANKYO_SCRUB_MODE` | `mask` | `mask` / `hash`（HMAC 擬似名化）/ `off` |
| `ZANKYO_MAX_EVENT_KB` | `256` | イベント保存の上限（超過は先頭のみ + `truncated`） |
| `ZANKYO_FLUSH_BUDGET_MS` | `1200` | 失敗レコードの PutObject 上限時間（呼び出し中の記録と SHUTDOWN フラッシュ）。失敗時の応答はこの時間まで遅れうる |
| `ZANKYO_PUT_TIMEOUT_MS` | `5000` | spill 再送の PutObject 上限時間（起動時・定期回収） |
| `ZANKYO_SPILL_DIR` | `/tmp/zankyo/<function>` | `.inflight` ステージと、PUT 前に先書きする失敗レコード（未送信分は再送まで残る）の置き場（既定は関数名でスコープ） |
| `ZANKYO_SPILL_MAX_FILES` | `64` | spill 保持数の上限。超過分は古いものから破棄 |
| `ZANKYO_SPILL_RETRY_MS` | `60000` | spill 再送を試みる間隔（生存中の定期回収） |
| `ZANKYO_SPILL_MAX_AGE_SECS` | `604800` | spill ファイルの有効期間。超過分は再送せず破棄 |
| `ZANKYO_MAX_BODY_KB` | `8192` | Runtime API が受け付けるボディ上限（KiB） |
| `ZANKYO_EXT_BODY_KB` | `1024` | Extensions API イベントボディ上限（KiB） |
| `ZANKYO_REGISTER_TIMEOUT_MS` | `10000` | extension 登録の上限時間 |
| `ZANKYO_EXT_RETRY_MS` | `500` | event/next ポーリング失敗時の再試行間隔 |
| `ZANKYO_EXT_MAX_POLL_FAILURES` | `120` | ポーリング連続失敗の上限（超過でループを抜ける） |
| `ZANKYO_SSM_TIMEOUT_MS` | `2000` | SSM get_parameter の上限時間。Lambda の Init 上限（10 秒）に含まれる |
| `ZANKYO_FORWARD_TIMEOUT_MS` | `60000` | `/next` 以外の上流転送の上限時間 |
| `ZANKYO_DISABLED` | `false` | 緊急停止スイッチ（passthrough） |

※「必須」は記録を有効にする条件です。未設定でも関数は正常に動きます。

## データ仕様

### S3 レイアウト

```
s3://{ZANKYO_BUCKET}/zankyo/{function-name}/{yyyy}/{mm}/{dd}/{requestId}.json
```

### レコード形式（1 ファイル = 1 失敗呼び出し）

```json
{
  "version": "1",
  "functionName": "my-api",
  "functionVersion": "12",
  "requestId": "...",
  "invokedAt": "2026-09-22T12:34:56Z",
  "failureType": "handler_error | init_error | timeout",
  "event": { "…scrub 済みイベント…" },
  "response": { "…scrub 済み応答または error オブジェクト…" },
  "errorContext": { "errorType": "...", "errorMessage": "...", "stackTrace": "..." },
  "scrubReport": { "fieldsRedacted": 12, "patternsApplied": ["email","jwt"] }
}
```

補助フィールド（該当時のみ付与）:

- `eventIsRawText: true` — 元イベントが JSON でない生テキストの場合。
  `event` は文字列として保持され、fixture はそのまま書き出します。
- `eventIsBase64: true` — 元イベントが UTF-8 でないバイナリの場合。
  `event` は base64 文字列として保持され、fixture はそのまま書き出します。
- 上の 2 つ（非 JSON イベント）は、replay/redrive/diff の対象外です（exit 4）。
  Lambda の [Invoke API](https://docs.aws.amazon.com/lambda/latest/api/API_Invoke.html)
  は JSON のペイロードしか受け付けず、JSON でない本文を
  `InvalidRequestContentException` で拒否するためです。
- `truncated: true` — イベントが `ZANKYO_MAX_EVENT_KB` を超えた場合。
  先頭のみ保持のため fixture/replay/redrive/diff はすべて対象外
  （部分イベントの replay は誤結果を生むため拒否）。

## PII scrub（既定 ON）

ハイブリッド方式で、消した内容を `scrubReport` に証跡化します。

- **フィールド名 denylist**: `password, secret, token, apiKey, authorization,
  privateKey, sessionId, cookie, cookies, ssn, creditCard, cvv, pin`（大文字小文字・
  セパレータ `_` `-` `.` 空白は不問。`access_token`→`token` や
  `Set-Cookie`→`cookie` のような prefix/suffix 一致も対象）＋
  `ZANKYO_SCRUB_FIELDS`。値が配列なら要素ごとに置き換え、配列の形を保ちます。
- **文字列化された JSON**（API Gateway・Function URL の `body` 等）も、
  中のフィールドに denylist を適用します。一致した場合だけ、その文字列を
  空白なし・キーは辞書順の JSON に書き直します。
- **パターン検出**: email / クレカ番号（**Luhn 検証付き**）/ JWT /
  AWS アクセスキー / Bearer トークン / 電話番号 / IPv4。
  対象は文字列値だけです。数値型の値（`"cardNo": 4111111111111111` 等）と
  オブジェクトのキー名（`{"alice@example.com": …}` 等）は検査しません。
  該当するフィールド名を `ZANKYO_SCRUB_FIELDS` に足すと、値ごとマスクされます。
- **自由テキストには denylist が効きません**。`errorContext` の `errorMessage`・
  `stackTrace` や、エラー応答の中のメッセージ・スタックトレースの文字列には、
  パターン検出だけが掛かります（`ZANKYO_SCRUB_FIELDS` も効きません）。
  `throw new Error('invalid: ' + JSON.stringify(event))` のように例外メッセージへ
  イベントを埋め込むと、`password` などの値が平文のまま S3 に残ります。
  例外メッセージには、イベントや秘密の値を入れないでください。
- **mask モード**は形状保持（`j***@e***.com`、`***1234`）で再現性を維持。
- **hash モード**は HMAC-SHA256 擬似名化（鍵は関数名+バケット由来の
  決定的 seed。暗号化ではなく「同じ値→同じハッシュ」の再現性が目的）。
- 無効化は `ZANKYO_SCRUB_MODE=off` の明示設定のみ。

## セキュリティ

- レコードは**利用者自身のアカウントの S3** にのみ保存。外部送信ゼロ。
- 関数に付与するのは `s3:PutObject` のみ（+KMS 時は Encrypt/GenerateDataKey）。
  書き込み先は `zankyo/{function-name}/` 配下に限るので、バケットを共有する
  別の関数のレコードは書けません（CDK construct の場合）。
  `ZANKYO_SSM_PARAM` を使う場合だけ、`ssm:GetParameter` を別に付与します（「設定」参照）。
- CLI は、キーの関数名と本文の `functionName` が食い違うレコードを
  replay・diff・redrive に使いません（exit 4）。
- proxy が listen するのは `127.0.0.1` のみ。
- 依存は lockfile で固定（`Cargo.lock` / `pnpm-lock.yaml`）。
  CI で `cargo audit` と `pnpm audit` を実行します。
- `pnpm.auditConfig.ignoreGhsas` の GHSA-6cpc-mj5c-m9rq は
  ワークスペースの importer パス `cli/` が deprecated な npm パッケージ
  `cli`（<1.0.0）と誤照合されるための除外です（中身の package 名は
  `lambda-zankyo` で、実依存に `cli` は存在しません）。実依存に `cli` を
  追加する場合はこの除外を見直してください。

## 制限事項（重要）

- **async invoke は対象外**（Lambda Destinations を使ってください）。
- `AWS_LAMBDA_EXEC_WRAPPER` は 1 スロットのみ。他の wrapper ツール
  （aws-lambda-web-adapter 等）との**併用は未対応**です。
- `provided.*` ランタイムは bootstrap が exec wrapper を尊重する場合のみ。
- SnapStart・RESPONSE_STREAM・API Gateway 29s タイムアウト（Lambda は
  成功しているケース）は正式保証外（SPEC Open Questions）。
- timeout フラッシュはベストエフォート（shutdown ウィンドウ制約あり）。

## リポジトリ構成

```
proxy/       # Rust: Runtime API proxy + external extension（単一バイナリ）
cli/         # TypeScript CLI (node --test、AWS SDK v3、引数は node:util)
construct/   # CDK construct (aws-cdk-lib v2、SAR 参照 + bucket/IAM 配線)
examples/    # デモスタック（handler error / timeout / init error）
sar/         # SAR 公開用 SAM テンプレート
scripts/     # build-layer.mts（musl 静的バイナリ → layer zip）、check-versions.mts（版の一致）
SPEC.md      # 仕様書（決定事項・スコープ外・Open Questions）
RELEASING.md # リリースの手順（SAR・npm・GitHub Release）
```

## 開発

```bash
pnpm install            # JS/TS 依存
pnpm gate               # lint + 版の一致 + build + typecheck + test（全パッケージ）
cargo test --locked --workspace   # Rust のユニットテストと統合テスト
cargo fmt --all -- --check && cargo clippy --locked --workspace --all-targets -- -D warnings
```

- TS: strict + `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes`、
  Biome でフォーマット統一。テストは `node --test`（外部サービス不要）。
  `scripts/*.mts` は Node が型を除去して直接実行します。型は `pnpm typecheck` が
  `scripts/tsconfig.json` で検査します。
- Rust: ロジック（scrub/record/config/inflight）は IO と分離した
  ユニットテスト。proxy/extension 経路はモック Runtime API/S3 への
  統合テスト（`proxy/tests/`）で検証。
- 版: リリースでは、`cli/package.json`・`construct/package.json`・
  `proxy/Cargo.toml`（と `Cargo.lock`）・`sar/template.yaml` の `SemanticVersion` を
  同じ版に上げます。SAR の `SourceCodeUrl` はその版のタグを指し、`CHANGELOG.md` には
  その版の見出しを置きます。`pnpm check:versions`（`pnpm gate` と CI が実行）で確かめます。
  construct の既定の SAR 版は、construct のテストが `sar/template.yaml` と突き合わせます。
  公開までの手順は [RELEASING.md](https://github.com/seike460/lambda-zankyo/blob/main/RELEASING.md) にあります。

### 保守性の指針

設定は env → SSM overlay → 既定値の順で解決され、動作ノブはすべて
`ZANKYO_*` 環境変数で外から変えられます（一覧は上の表）。拡張は
データの 1 エントリ追加で済みます: scrub パターン・CLI コマンド・
SSM キー・build arch。spill は atomic 書き込み・上限・定期回収・
送れたファイルの削除までライフサイクルが閉じており、rerun で
残滓が増えません。不変条件の全体は ARCHITECTURE.md を参照してください。

### 再現性（reproducibility）

- 依存は lockfile 固定: `Cargo.lock`（Rust）と `pnpm-lock.yaml`（JS/TS）
  をコミット済み。`pnpm install --frozen-lockfile` / `cargo build --locked`
  で同一依存が解決されます。CI のビルド・テストと `scripts/build-layer.mts`
  も `--locked` を付けます。`Cargo.toml` と lockfile がずれていれば失敗します。
- ツールチェーンも固定: `rust-toolchain.toml`（Rust 1.98.1。最低要件は
  `proxy/Cargo.toml` の `rust-version` = 1.94.1 で、CI で検証）、`.nvmrc`
  （Node 24。engines の下限 `>=24` と同じで、CI は Node 24 と 26 で検証）、
  `package.json` の `packageManager`（pnpm 10.13.1）。
- CI の全 GitHub Actions はコミット SHA でピン留めしています
  （`.github/workflows/ci.yml` の `@<sha> # vN` コメント参照）。
  aarch64 をビルドする cross のコンテナは、`Cross.toml` で digest に固定しています。
- `scripts/build-layer.mts` は決定的 zip を生成します: エントリの
  mtime を最古（zip 表現の下限 1980-01-01）に揃え、`zip -X` で
  拡張属性を捨て、エントリ順を固定の引数順で渡します。
  同一環境では同一バイナリから同一 zip が出ます
  （"version made by" バイトは OS 依存のためクロスプラットフォームの
  バイト一致は対象外）。

## License

MIT

Layer の zip は `share/licenses/zankyo/`（Lambda 上では `/opt/share/licenses/zankyo/`）に、
この LICENSE と `THIRD_PARTY_LICENSES` を含みます。`THIRD_PARTY_LICENSES` は、
バイナリに入る Rust crate のライセンスと著作権表示の原文です。
