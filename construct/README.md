# zankyo-cdk

`lambda-zankyo` の Layer を Lambda 関数に取り付ける AWS CDK construct です。
Layer の参照（SAR）・記録用バケット・環境変数・IAM 権限を、まとめて配線します。
記録の対象は、同期（RequestResponse）呼び出しの失敗です。Layer と CLI を含む全体は、
[リポジトリ直下の README](https://github.com/seike460/lambda-zankyo#readme) を参照してください。

## install

```bash
npm install zankyo-cdk   # peer dependency: aws-cdk-lib（v2）と constructs（v10）
```

## usage

```ts
import { Zankyo } from 'zankyo-cdk';

const zankyo = new Zankyo(this, 'Zankyo');
zankyo.attachTo(myFunction);
```

`attachTo(fn)` は、関数に次の設定をします。

- zankyo の Layer を追加する
- `AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo-wrapper` と `ZANKYO_BUCKET` を設定する
  （指定があれば `ZANKYO_KMS_KEY`・`ZANKYO_SCRUB_FIELDS` も）
- 記録先 `zankyo/{関数名}/*` への `s3:PutObject` を付与する
  （`kmsKey` 指定時は `kms:Encrypt`・`kms:GenerateDataKey` も）

`bucket` を渡さない場合は、パブリックアクセス全ブロック・SSL 必須・
ライフサイクル（既定 30 日）のバケットを作ります。スタックを削除しても、
このバケットは残ります（`RemovalPolicy.RETAIN`）。

## props

| prop | 既定 | 用途 |
|---|---|---|
| `bucket` | 新規作成 | 失敗レコードの保存先 |
| `recordRetentionDays` | `30` | 新規バケットのレコード保持日数（1 以上の整数）。`bucket` 指定時は無関係 |
| `kmsKey` | なし（SSE-S3） | SSE-KMS に使うキー |
| `scrubFields` | なし | 既定の denylist に足すフィールド名（`ZANKYO_SCRUB_FIELDS`） |
| `sarApplicationId` | 公開中の SAR アプリ | SAR アプリケーションの ARN |
| `semanticVersion` | この construct に対応する版 | SAR アプリの版 |
| `layer` | なし（SAR を使う） | セルフホストの LayerVersion。指定時は SAR をデプロイしない |
| `arm64` | `false` | SAR の arm64 用 Layer を使う。`layer` 指定時は無関係 |

## Layer の入手先

- **SAR（既定）**: `arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo`。
  公開アプリなので、ap-northeast-1 以外のリージョンのスタックからもデプロイできます
  （[AWS ドキュメント](https://docs.aws.amazon.com/serverlessrepo/latest/devguide/serverlessrepo-publishing-applications.html)）。
- **セルフホスト**: GitHub Releases の zip を Lambda Layer として発行し、
  `layer` に渡します。

## arm64 の関数

SAR の Layer は、`arm64` の指定で x86_64 用と arm64 用を切り替えます。
関数のアーキテクチャと食い違うと、`attachTo` が例外を投げます。
別のアーキテクチャのバイナリは起動できず、関数ごと動かなくなるためです。
両方のアーキテクチャの関数があるときは、Zankyo をアーキテクチャごとに作り、
バケットを共有します。

```ts
const zankyo = new Zankyo(this, 'Zankyo');
const zankyoArm = new Zankyo(this, 'ZankyoArm', { arm64: true, bucket: zankyo.bucket });
zankyo.attachTo(x86Function);
zankyoArm.attachTo(arm64Function);
```

## License

MIT
