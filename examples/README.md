# examples — zankyo デモアプリ

3 種類の失敗（handler error / timeout / init error）を起こす関数を、
`zankyo-cdk` construct 付きでデプロイする検証用スタック。
SPEC.md「E2E 検証手順」の実施先。

## deploy

```bash
pnpm install
pnpm --filter zankyo-examples deploy
```

## 検証の流れ

```bash
# 1. handler error を直接 invoke（RequestResponse = 同期呼び出し）
aws lambda invoke --function-name ZankyoDemo-ThrowerXXX \
  --payload '{"email":"a@b.com","password":"x"}' out.json

# 2. timeout ケース（3s でタイムアウトする関数）
aws lambda invoke --function-name ZankyoDemo-SleeperXXX --payload '{}' out.json

# 3. レコードが S3 にあることを確認
export ZANKYO_BUCKET=<デプロイで作られたバケット>
zankyo list --since 1h

# 4. fixture → sam local で再現
zankyo fixture --last --out event.json
sam local invoke -e event.json

# 5. 関数を修正して新バージョン → 差分比較
zankyo diff <requestId> --alias old --alias new

# 6. 修復後に本番再投入（既定 dry-run）
zankyo redrive <requestId> --confirm
```

## 注意

- `InitError` はデプロイ直後の初回起動でしか init error を起こさない。
  実行環境が再利用されると再度 init error にならない点に注意
  （新しい実行環境の確保は、関数設定の軽微な変更→保存で誘発できる）。
- timeout の SHUTDOWN フラッシュはベストエフォート。
  取りこぼした場合は `/tmp/zankyo/` spill または頻度計測の対象。
