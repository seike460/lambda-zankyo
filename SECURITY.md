# Security Policy

## 報告

脆弱性を見つけた場合は公開 Issue ではなく、GitHub の
[Security Advisories](https://github.com/seike460/lambda-zankyo/security/advisories)
から非公開で報告してください。初期応答は 7 日以内を目安にします。

## サポート範囲

最新のリリースタグのみを対象とします。

## セキュリティモデル

zankyo は失敗イベントを扱うため、記録経路そのものを最小権限で設計しています。

- **保存データ**: 失敗した invocation のイベントとエラー文脈のみ。
  成功 invocation は一切保存しません。
- **PII scrub**: 既定で有効。フィールド名 denylist とパターン検出
  （email・JWT・クレジットカード番号等）の hybrid で、
  `ZANKYO_SCRUB_MODE=off` を明示しない限り無効化できません。
- **権限**: CDK construct が付与するのは記録バケットの
  `zankyo/{function-name}/` 配下への `s3:PutObject` のみ。読み取り権限は
  関数に付きません。バケットを共有する関数どうしも、互いのレコードは書けません。
- **暗号化**: SSE-S3 が既定。`ZANKYO_KMS_KEY` で SSE-KMS を選択でき、
  その場合に限り KMS の encrypt / data key 権限が付きます。
- **fail-open**: proxy 自身の障害で関数本体を止めない設計です。
  記録経路が死んでも handler 実行は継続します。
- **secret の取り扱い**: 環境変数と SSM Parameter が唯一の設定供給元で、
  認証情報をファイルに書き込みません。CLI のエラーメッセージには
  イベント本文・認証情報を含めません。
- **/tmp 上の一時ファイル**: timeout 捕捉のため呼び出し中のイベントを
  `/tmp/zankyo/<function>/zankyo-*.inflight` へ生のままステージします
  （scrub 前のイベントが一時的にディスク上に存在します）。また S3
  への PUT 失敗時は scrub 済みレコードを同 dir へ退避します。
  いずれも mode 0600・zankyo 作成時は 0700 の関数スコープ dir・
  呼び出し完了/送信成功で削除で、書き込み先は同一 sandbox 内に
  限られます。

## 既知の制約

- Runtime API proxy は `localhost` のみを中継します。
  外部から到達可能な経路は持ちません。
- SHUTDOWN 通知からの残り時間内に S3 への PutObject が完了しない場合、
  そのイベントは `/tmp/zankyo` へのローカル退避のみで取りこぼす可能性が
  あります（ベストエフォート、仕様上の既知制約）。
- scrub はヒューリスティックです。denylist に無い秘密情報は
  `ZANKYO_SCRUB_FIELDS` で必ず追加してください。パターン検出は
  文字列値だけが対象で、数値型の値とオブジェクトのキー名は検査しません。
