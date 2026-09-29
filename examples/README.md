# examples — zankyo デモアプリ

3 種類の失敗（handler error / timeout / init error）を起こす関数を、
`zankyo-cdk` construct 付きでデプロイする検証用スタック。
SPEC.md「E2E 検証手順」の実施先。

## deploy

リポジトリルートで実行:

```bash
pnpm install
pnpm build                            # zankyo-cdk の dist を生成（workspace 依存の解決に必要）
pnpm --filter zankyo-examples run deploy
```

`cdk synth` のみ試す場合も同じ前提です（`pnpm build` 後に
`pnpm --filter zankyo-examples run synth`）。

## 検証の流れ

```bash
# 1. handler error を直接 invoke（RequestResponse = 同期呼び出し）
#    AWS CLI v2 は --payload を base64 と解釈するため、JSON のまま渡す指定を付ける
aws lambda invoke --function-name ZankyoDemo-ThrowerXXX \
  --cli-binary-format raw-in-base64-out \
  --payload '{"email":"a@b.com","password":"x"}' out.json

# 2. timeout ケース（3s でタイムアウトする関数）
aws lambda invoke --function-name ZankyoDemo-SleeperXXX \
  --cli-binary-format raw-in-base64-out --payload '{}' out.json

# 3. レコードが S3 にあることを確認
export ZANKYO_BUCKET=<デプロイで作られたバケット>
zankyo list --since 1h

# 4. fixture → sam local で再現（Docker が必要）
zankyo fixture --last --function ZankyoDemo-ThrowerXXX --out event.json
pnpm --filter zankyo-examples run synth
echo '{"Parameters":{"AWS_LAMBDA_EXEC_WRAPPER":""}}' > env.json
sam local invoke -t examples/cdk.out/ZankyoDemo.template.json Thrower \
  -e event.json --env-vars env.json

# 5. 関数を修正して新バージョン → 差分比較
zankyo diff <requestId> --alias old --alias new

# 6. 修復後に本番再投入（既定 dry-run）
zankyo redrive <requestId> --confirm
```

## 注意

- `InitError` はデプロイ直後の初回起動でしか init error を起こさない。
  実行環境が再利用されると再度 init error にならない点に注意
  （新しい実行環境の確保は、関数設定の軽微な変更→保存で誘発できる）。
- 手順 4 の `--last` は全関数で最新のレコードを選ぶ。手順 2 の timeout レコードを
  避けるため、`--function` で Thrower に絞る。
- 手順 4 の `sam local invoke` は、関数を construct ID（`Thrower`）で指定する。
  sam local は SAR の Layer を取得しないため、`AWS_LAMBDA_EXEC_WRAPPER` が
  存在しない `/opt/zankyo-wrapper` を指したままになり、ランタイムが起動しない。
  `env.json` はこの変数を空にして wrapper を外す。
- sam はテンプレートの SAR アプリ（`AWS::Serverless::Application`）を解決するため、
  Serverless Application Repository の API を呼ぶ。デプロイと同じ AWS 認証情報と
  リージョンの設定で実行する。
- デモのハンドラは `handlers/` のファイルで持つ。sam local は inline code
  （`Code.fromInline`）の関数を実行しない。
- 手順 1 の `password` は、レコードの `event` の中で denylist によりマスクされる。
  例外メッセージには denylist が効かないため（README「PII scrub」）、デモの例外メッセージには
  イベントを入れていない。
- timeout の SHUTDOWN フラッシュはベストエフォート。shutdown ウィンドウ内に
  PutObject が終わらないと、レコードは spill（既定 `/tmp/zankyo/<関数名>`）に残る。
  timeout 後の reset は `/tmp` を消さないため、同じ実行環境が次の呼び出しで
  init し直すと、起動時の回収が S3 へ再送する。実行環境がそのまま破棄されると、
  そのレコードは届かない。
