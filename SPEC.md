# lambda-zankyo（残響）— 仕様書

sync invoke で失敗した Lambda 呼び出しの「イベント＋エラー応答」を確実に残し、
ローカル再現・差分リプレイ・本番再実行まで担う Layer＋CLI の OSS。

## 背景と存在理由

- Lambda Destinations は **async invoke / stream invoke 限定**。sync invoke（API Gateway・ALB・Function URL・直接 invoke・Cognito 等）で失敗した呼び出しのイベントを保存する公式経路は**ゼロ**（AWS re:Post で確認済み）。
- sync の失敗は「エラーはログに残るが、入力イベントは蒸発する」。handler が event をログしていなければ再現不能。
- AWS が sync 向けに payload を永続化するフックを持たないのは構造的（sync は応答が呼び出し元に返る設計。AWS の責任分界は「エラーを返す、リトライは client」）。10 年追加されていない。
- Powertools `logEvent`・Middy はコード変更前提。Datadog は商用で fixture 出力・sam local 連携・差分リプレイなし。Keploy は開発時記録が主眼で eBPF は Lambda 環境内で不可。

## 決定事項

| 項目 | 決定 |
|---|---|
| 名称 | **lambda-zankyo**（残響） |
| プロキシ実装 | **Rust 単一バイナリ**（musl 静的リンクで実行時の共有ライブラリ依存なし、x86_64/arm64） |
| timeout 捕捉 | **Extension API の SHUTDOWN イベント**（reason=timeout）で in-flight イベントをフラッシュ |
| PII scrub | **ハイブリッド**（フィールド名 denylist＋パターン検出）、既定 ON |
| 配布 | **SAR（Serverless Application Repository）公開** |
| CLI スコープ | **fixture 生成＋差分リプレイ＋本番再実行（redrive）** まで MVP |
| 設定供給 | **両方**（環境変数を既定、SSM Parameter 指定時はそちら優先） |
| CDK | **construct 同梱**（Layer 参照＋S3 bucket＋IAM を一発配線） |

## アーキテクチャ

```
Lambda Service ──Runtime API──▶ zankyo proxy (Rust, Layer) ──▶ 実ランタイム(handler)
                     ▲                │        └─ /tmp に .inflight ステージ共有
                     │                ├─ 失敗時のみ: イベント+エラー → scrub → S3(+KMS)
                     │                └─ zankyo agent (/opt/extensions 起動)
                     │                     └─ Extensions API /register（SHUTDOWN 受信用）
                     └── AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper で差し込み
```

- Layer 内の単一バイナリが **Runtime API proxy と external extension を兼務**する（起動方法でモードが分かれる）。プロセス構成とデータフローの詳細は ARCHITECTURE.md。
  - proxy: exec wrapper から起動し、`AWS_LAMBDA_RUNTIME_API` を localhost の自分に向け、`/next`・`/response`・`/error`・`/init/error` を中継。
  - extension（agent）: layer の `/opt/extensions/zankyo` から platform が別プロセスとして起動する（internal extension には SHUTDOWN が届かないため）。`/extension/register` して `/event/next` をポーリングし、INVOKE/SHUTDOWN（reason=timeout/failure/spindown）を受信。
- **成功呼び出しは記録しない**（S3 PUT・spill なし）。`/next` のイベントは timeout 捕捉のため `/tmp` に一時ステージし、完了時に削除する。
- 失敗の定義: `/error` 呼出・`/init/error`・`/response` 内の errorType 含有・SHUTDOWN の時点で応答を返していない呼び出し（reason を問わず failureType=timeout。reason は errorContext.errorType に写す）。
- timeout 捕捉: proxy が in-flight イベントを `/tmp` の `.inflight` にステージ → SHUTDOWN を受けた extension がそれを読み、S3 へベストエフォートフラッシュ（shutdown ウィンドウ内に PutObject が完了しない場合は取りこぼす旨を README に明記）。
- 対象ランタイム: `AWS_LAMBDA_EXEC_WRAPPER` を尊重する全 managed runtime（nodejs/python/java/dotnet/ruby）。`provided.*` は bootstrap が exec wrapper を尊重する場合のみ。

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
  "event": { "...scrub 済みイベント..." },
  "response": { "...scrub 済み応答または error オブジェクト..." },
  "errorContext": { "errorType": "...", "errorMessage": "...", "stackTrace": "..." },
  "scrubReport": { "fieldsRedacted": 12, "patternsApplied": ["email","jwt"] }
}
```

- scrub は **記録に `scrubReport` を併記**して「何をどう消したか」を監査可能にする（再現の障害にならない範囲で構造は保持）。
- `ZANKYO_MAX_EVENT_KB`（既定 256）を超えるイベントは先頭だけを保持し、`truncated: true` を付ける。`truncated` のレコードは fixture・replay・diff・redrive の対象外。

## 設定（環境変数。`ZANKYO_SSM_PARAM` 指定時は SSM の JSON を優先）

| env | 既定 | 用途 |
|---|---|---|
| `ZANKYO_BUCKET` | （必須） | 失敗レコードの保存先 |
| `ZANKYO_KMS_KEY` | SSE-S3 | 暗号化キー |
| `ZANKYO_SSM_PARAM` | なし | 設定 JSON を保持する SSM Parameter 名。指定時は env より優先 |
| `ZANKYO_SCRUB_FIELDS` | 既定 denylist | 追加フィールド名（カンマ区切り） |
| `ZANKYO_SCRUB_MODE` | `mask` | `mask` / `hash`（HMAC 擬似名化）/ `off` |
| `ZANKYO_MAX_EVENT_KB` | `256` | イベント保存の上限 |
| `ZANKYO_DISABLED` | `false` | 緊急停止スイッチ |

### scrub 規則（ハイブリッド）

- **フィールド名 denylist**: `password, secret, token, apiKey, authorization, privateKey, sessionId, cookie, cookies, ssn, creditCard, cvv, pin`（大文字小文字・セパレータ不問）＋ `ZANKYO_SCRUB_FIELDS`。文字列化された JSON（API Gateway の `body` 等）の中のフィールドにも適用する。
- **パターン検出**: email / クレカ番号（Luhn 検証付き）/ JWT / AWS アクセスキー / Bearer トークン / 電話番号 / IPv4。対象は文字列値のみ（数値型の値・キー名は対象外）。
- 既定 mask は `***` ではなく `j***@e***.com` 型の**形状保持マスク**（再現性を損なわないため）。

## CLI（TypeScript strict + Biome。npm パッケージ `lambda-zankyo`、コマンド名 `zankyo`。`npx lambda-zankyo` でも可）

| コマンド | 内容 |
|---|---|
| `zankyo list [--function X] [--since 24h]` | S3 の失敗レコード一覧 |
| `zankyo fixture <requestId\|--last> [--out file]` | レコード → `sam local invoke -e` 用 JSON |
| `zankyo replay <requestId> [--alias A]` | 指定バージョンへ同一イベントを再実行し応答を表示 |
| `zankyo diff <requestId> --alias a --alias b` | 新旧バージョンに投げて応答を比較（exit code で一致/差異を返し CI に使える） |
| `zankyo redrive <requestId> [--confirm]` | 修復後に本番関数へ再投入（dry-run 既定） |

- AWS SDK v3。`--profile` / `--region` 対応。出力は人間向け表＋`--json` 両対応。

## CDK construct（aws-cdk-lib v2、TypeScript strict + Biome）

```ts
new Zankyo(this, 'Zankyo', { bucket?, kmsKey?, scrubFields? })
  .attachTo(fn); // fn.layers += zankyo layer, env 注入, s3:PutObject + kms 権限を role へ
```

- Layer は SAR アプリケーションを参照（セマンティックバージョン指定）。bucket 未指定なら新規作成（Object Lock ではなく lifecycle 30 日を既定提案）。

## 配布

- **Layer**: SAR で公開（1 クリック導入。`arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo`）。GitHub Releases にも zip を添付（セルフホスト用）。
- **CLI**: npm パッケージ `lambda-zankyo`（コマンド名 `zankyo`）。
- **Construct**: npm パッケージ `zankyo-cdk`。
- 公開の手順は RELEASING.md。
- リポジトリ構成（monorepo）:
  ```
  lambda-zankyo/
    proxy/      # Rust（cross または cargo build で x86_64/arm64 の musl 静的バイナリ。梱包は scripts/build-layer.mts）
    cli/        # TypeScript + Biome + AWS SDK v3
    construct/  # TypeScript + Biome + aws-cdk-lib
    examples/   # 失敗するデモ関数（throw / timeout / init error）
    SPEC.md
  ```

## スコープ外（やらないこと）

- **async invoke の失敗** — Lambda Destinations に任せる。README 冒頭に明記。
- 成功呼び出しの観測・型生成・スキーマドリフト検知（v2 以降の拡張余地）。
- OS 専用ランタイム（`provided.*`）での exec wrapper 非対応ケース。
- 他の exec wrapper ツール（failure-lambda・aws-lambda-web-adapter 等）との **wrapper 競合の chain 対応**（`AWS_LAMBDA_EXEC_WRAPPER` は 1 スロットのみ。非対応を README に明記。chain は Open Questions）。
- SnapStart・Lambda Managed Instances・RESPONSE_STREAM 呼出の正式保証（検証対象として Open Questions に列挙）。

## セキュリティ要件

- scrub は**既定 ON**、無効化は `ZANKYO_SCRUB_MODE=off` の明示のみ。`scrubReport` で適用結果を証跡化。
- レコードの保存先は**利用者自身のアカウントの S3**（外部送信ゼロ）。IAM 権限は `s3:PutObject` 限定。
- 依存は lockfile（`Cargo.lock` / `pnpm-lock.yaml`）で固定し、CI で `cargo audit` / `pnpm audit` を実行する。

## Open Questions

1. exec wrapper の chain（`ZANKYO_CHAIN_WRAPPER=/opt/other-wrapper` で次の wrapper を呼ぶ方式）は実現可能か。
2. SnapStart 環境で extension ライフサイクルが変わるか（init タイミングのずれ）。
3. RESPONSE_STREAM 呼出での失敗応答捕捉（チャンク応答中の error）。
4. timeout 時の SHUTDOWN ウィンドウに PutObject が間に合うか実測。/tmp にも残して次の init で回収する二重化は実装済み（`.inflight` ステージと write-ahead spill。ARCHITECTURE.md のデータフロー参照）。
5. API Gateway 29s timeout のような「呼び出し元側 timeout」（Lambda は成功している）の扱い。

## E2E 検証手順

1. `examples/` のデモ関数（`zankyo demo deploy` 相当の CDK app）をデプロイ。
2. 失敗するイベント・timeout するイベントを直接 invoke（RequestResponse）で発火。
3. `s3 ls zankyo/...` にレコードがあり、`event`/`errorContext`/`scrubReport` が揃っていること。PII フィールドがマスクされていること。
4. `zankyo fixture --last --function <失敗する関数>` で `sam local invoke -e` が通り、同じエラーが再現すること（`--last` は全関数で最新のレコードを選ぶため、`--function` を付けないと timeout レコードを拾いうる）。
5. 関数を修正して新バージョンを発行 → `zankyo diff <id> --alias old --alias new` で差異が出ること。
6. `zankyo redrive <id> --confirm` で修復後の本番関数が成功応答を返すこと（`--confirm` を付けないと dry-run で、関数を呼ばない）。
7. timeout ケースで SHUTDOWN フラッシュによるレコードが残ること（取りこぼしがあれば頻度を計測・文書化）。
