# Releasing

リリースの手順です。例として版を `0.1.1` と書きます。公開先は次の 4 つです。

- SAR: `arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo`
- npm: `zankyo-cdk`（construct）と `lambda-zankyo`（cli）
- GitHub Release: Layer の zip 2 つと `SHA256SUMS`

公開する成果物は、すべてタグを打った commit から作ります。v0.1.0 は、SAR をタグの後の
commit から公開したため、タグと SAR のメタデータが食い違いました（CHANGELOG 参照）。

## 準備

- docker と cross 0.2.5（`cargo install cross --locked --version 0.2.5`）。
  cross のコンテナは、x86_64 と aarch64 の両方を `Cross.toml` が digest で固定しています。
- SAM CLI と、アカウント 446537410535 の AWS 認証情報。
- `sam package` が成果物を置く S3 バケット。公開するリージョン（ap-northeast-1）に作り、
  SAR が読めるようにバケットポリシーを付けます
  （[AWS SAM CLI での公開](https://docs.aws.amazon.com/serverless-application-model/latest/developerguide/serverless-sam-template-publishing-applications.html)）。

  ```json
  {
    "Version": "2012-10-17",
    "Statement": [
      {
        "Effect": "Allow",
        "Principal": { "Service": "serverlessrepo.amazonaws.com" },
        "Action": "s3:GetObject",
        "Resource": "arn:aws:s3:::<バケット名>/*",
        "Condition": { "StringEquals": { "aws:SourceAccount": "446537410535" } }
      }
    ]
  }
  ```

- 2 つの npm パッケージの publish 権限と、`gh` の認証。

## 1. 版をそろえる

リリース用のブランチで、次の 7 か所を同じ commit で新しい版にします。

1. `cli/package.json` の `version`
2. `construct/package.json` の `version`
3. `proxy/Cargo.toml` の `version`
4. `Cargo.lock` の `zankyo`（`cargo update -p zankyo` で更新します）
5. `sar/template.yaml` の `SemanticVersion` と `SourceCodeUrl`（`/tree/v0.1.1`）
6. `construct/src/zankyo.ts` の `DEFAULT_SEMANTIC_VERSION`
7. `CHANGELOG.md`。`## [Unreleased]` の中身を `## [0.1.1] - YYYY-MM-DD` へ移し、
   空の `## [Unreleased]` を上に残します。末尾のリンクも
   `[Unreleased]: .../compare/v0.1.1...HEAD` と `[0.1.1]: .../releases/tag/v0.1.1` にします。

`pnpm gate` の `check:versions` が 1〜5 と 7 の見出しを、construct のテストが 6 を確かめます。
`examples/package.json` は公開しないので対象外です。次のコマンドがすべて通ることを確かめます。

```bash
pnpm install --frozen-lockfile
pnpm gate
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
pnpm audit --prod
```

## 2. main にマージしてタグを打つ

PR の CI がすべて通ってから main にマージします。main の CI も通ったことを確かめます。

```bash
git switch main
git pull --ff-only
git status --porcelain            # 何も出ないこと
git tag -a v0.1.1 -m "v0.1.1"
git push origin v0.1.1
```

以降の手順は、この commit（main の先端＝タグ）のまま、作業ツリーを変えずに行います。

## 3. Layer の zip を作る

```bash
BUILDER="cross build" node scripts/build-layer.mts
(cd dist/layer && shasum -a 256 zankyo-x86_64.zip zankyo-aarch64.zip > SHA256SUMS)
```

`dist/layer/zankyo-{x86_64,aarch64}.zip` と、SAR 用の同じ zip
`sar/dist/layer-{x86_64,aarch64}.zip` ができます。

`BUILDER="cross build"` を付けると、両 arch とも `Cross.toml` で固定したコンテナでビルドします。
cross が無いときは、手元の cargo に切り替えずに失敗します。

## 4. SAR に公開する

`sam publish` は、`sam package` 済みのテンプレートしか受け付けません。`sam package` が
Layer の zip と `LicenseUrl`・`ReadmeUrl` のファイルを S3 に上げ、そこを指す
テンプレートを書き出します。

```bash
sam package --template-file sar/template.yaml --s3-bucket <バケット名> \
  --output-template-file sar/packaged.yaml --region ap-northeast-1
sam publish --template sar/packaged.yaml --region ap-northeast-1
```

公開した版と、公開の設定が残っていることを確かめます。

```bash
app=arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo
aws serverlessrepo get-application --application-id "$app" --semantic-version 0.1.1 \
  --region ap-northeast-1 --query 'Version.[SemanticVersion,SourceCodeUrl]'
aws serverlessrepo get-application-policy --application-id "$app" --region ap-northeast-1
```

ポリシーには、`Principals` が `["*"]` の文があるはずです。

## 5. npm に公開する

construct の既定の SAR 版は、この版を指します。そのため、SAR の公開を確かめてから
construct を公開します。pack と publish の前に、`prepack` が dist をビルドし直します。
先に tarball を作り、`dist/`・`README.md`・`LICENSE`・`package.json` が入っていることを
確かめます。

```bash
tmp=$(mktemp -d)
(cd construct && pnpm pack --pack-destination "$tmp")
(cd cli && pnpm pack --pack-destination "$tmp")
for t in "$tmp"/*.tgz; do tar -tzf "$t"; done

(cd construct && pnpm publish)
(cd cli && pnpm publish)
npm view zankyo-cdk@0.1.1 version
npm view lambda-zankyo@0.1.1 version
```

npm の provenance（出どころの証明）は、GitHub Actions などのクラウド CI から
公開したときだけ付けられます。手元から公開するこの手順では付きません
（[npm Docs](https://docs.npmjs.com/generating-provenance-statements)）。

## 6. GitHub Release を作る

`CHANGELOG.md` のこの版の節を、リリースノートにします。

```bash
gh release create v0.1.1 --verify-tag --title v0.1.1 --notes-file <この版の節を書いたファイル> \
  dist/layer/zankyo-x86_64.zip dist/layer/zankyo-aarch64.zip dist/layer/SHA256SUMS
```

利用者は、`sha256sum -c SHA256SUMS`（macOS では `shasum -a 256 -c SHA256SUMS`）で
zip を確かめられます。
