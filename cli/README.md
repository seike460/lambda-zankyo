# lambda-zankyo (CLI)

`zankyo` — S3 に保存された「同期呼び出しの失敗レコード」を読み、
fixture 生成・差分リプレイ・本番再実行を行う CLI。
失敗レコードを作る Layer (proxy) 側と組み合わせて使う。詳細は
リポジトリ直下の README を参照。

## install

```bash
npm install -g lambda-zankyo
```

## usage

```bash
export ZANKYO_BUCKET=my-zankyo-records   # または --bucket

zankyo list --function my-api --since 24h
zankyo fixture --last --out event.json   # sam local invoke -e event.json
zankyo replay <requestId> --alias dev
zankyo diff <requestId> --alias v12 --alias v13   # exit 0/1 で CI 利用可
zankyo redrive <requestId> --confirm              # 既定は dry-run
```

全コマンド共通: `--bucket` / `--region` / `--profile` / `--json`。

## exit codes

| code | 意味 |
|---|---|
| 0 | 成功（diff: 応答一致 / redrive --confirm: 関数が成功応答） |
| 1 | diff: 応答に差異 / redrive: 関数がエラーを返した |
| 2 | 引数・設定ミス |
| 3 | AWS API 呼び出しの失敗 |
| 4 | レコードが見つからない / 形式不正 |
