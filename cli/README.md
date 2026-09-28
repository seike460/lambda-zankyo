# lambda-zankyo (CLI)

`zankyo` — S3 に保存された「同期呼び出しの失敗レコード」を読み、
fixture 生成・差分リプレイ・本番再実行を行う CLI。
失敗レコードを作る Layer (proxy) 側と組み合わせて使う。詳細は
[リポジトリ直下の README](https://github.com/seike460/lambda-zankyo#readme) を参照。

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

AWS API 呼び出しのタイムアウトは env で調整できます
（既定: 接続 5s・応答 30s。CI のハング防止）。

| env | 既定 | 用途 |
|---|---|---|
| `ZANKYO_CONNECT_TIMEOUT_MS` | `5000` | AWS API 接続の打ち切り |
| `ZANKYO_REQUEST_TIMEOUT_MS` | `30000` | AWS API 応答の打ち切り |
| `ZANKYO_LIST_PAGE_SIZE` | `200` | ListObjectsV2 の 1 ページ件数 |
| `ZANKYO_LIST_MAX_PAGES` | `50` | レコード探索のページ走査上限 |
| `ZANKYO_RECORD_MAX_MB` | `32` | レコード1件の読み取り上限（MiB） |

## exit codes

| code | 意味 |
|---|---|
| 0 | 成功（diff: 応答一致 / replay・redrive: 関数が成功応答） |
| 1 | diff: 応答に差異 / replay・redrive: 関数がエラーを返した |
| 2 | 引数・設定ミス |
| 3 | AWS API 呼び出しの失敗 |
| 4 | レコードが見つからない / 形式不正（キーと本文の関数名の食い違いを含む） / 再現不能（truncated・event 欠落） |
