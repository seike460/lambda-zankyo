//! 失敗レコードの永続化。S3 PutObject が本線、失敗時・SHUTDOWN 時は
//! /tmp への退避（spill）で取りこぼしを減らす。
//! このモジュールだけが S3/ファイルシステムに触れる。

use crate::config::Config;
use crate::error::{Result, ZankyoError};
use crate::inflight::{EventEncoding, Invocation};
use crate::record::{
    s3_key, to_json_bytes, truncate_event, ErrorContext, FailureRecord, FailureType,
    ScrubReportJson, RECORD_VERSION,
};
use crate::scrub::{ScrubReport, Scrubber};
use crate::spill;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ServerSideEncryption;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use time::OffsetDateTime;
use tracing::{info, warn};

/// `save` へ渡す入力一式。引数束ね。
pub struct SaveInput {
    pub request_id: String,
    pub invoked_at: OffsetDateTime,
    pub failure: FailureType,
    pub event: EventInput,
    pub response: Option<Value>,
    pub ctx: ErrorContext,
}

/// イベント本体とその保持形式。
pub struct EventInput {
    pub value: Option<Value>,
    /// 非 JSON イベントの保持形式（生テキスト / base64）。
    /// レコードの eventIsRawText / eventIsBase64 に写される。
    pub encoding: EventEncoding,
}

/// `stage_timeout` が返す PUT 待ちジョブ。
/// spill 済みのため、PUT が間に合わなくてもレコードは残る。
pub struct StagedRecord {
    pub request_id: String,
    pub key: String,
    pub body: Vec<u8>,
}

pub struct Recorder {
    s3: aws_sdk_s3::Client,
    cfg: Config,
    function_name: String,
    function_version: String,
    scrubber: Scrubber,
}

impl Recorder {
    pub fn new(
        s3: aws_sdk_s3::Client,
        cfg: Config,
        function_name: String,
        function_version: String,
    ) -> Self {
        // hash モードの鍵は秘密ではなく擬似名化の seed。
        // 同一デプロイ内で同じ値が同じハッシュになることを求めるため、
        // 関数名+バケットから決定的に作る。
        let seed = format!("{function_name}|{}", cfg.bucket);
        let scrubber = Scrubber::new(cfg.scrub_mode, &cfg.scrub_fields, &seed);
        Self {
            s3,
            cfg,
            function_name,
            function_version,
            scrubber,
        }
    }

    /// 起動時に確定した設定。extension 側のポーリングノブもここから読む。
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn flush_budget(&self) -> Duration {
        Duration::from_millis(self.cfg.flush_budget_ms)
    }

    fn put_timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.put_timeout_ms)
    }

    /// レコードを組み立てて保存する。`stage_save` + `commit_staged` の
    /// 連結（write-ahead → PUT → 成功で spill 削除）。
    pub async fn save(&self, job: SaveInput) {
        if let Some(staged) = self.stage_save(job) {
            self.commit_staged(&staged).await;
        }
    }

    /// 失敗レコードを組み立てて /tmp へ先書きする（write-ahead）。
    /// 呼び出し完了直後に実行環境は freeze されるため、S3 PUT は
    /// 「この呼び出しの unfrozen 時間内」に終わらせる必要がある。
    /// 先にローカルへ落とせば、PUT が間に合わなくても定期回収・
    /// 次回 init の recover_spills が拾える。
    /// シリアライズ失敗時のみ None（spill すら作れない）。
    pub fn stage_save(&self, job: SaveInput) -> Option<StagedRecord> {
        let request_id = job.request_id.clone();
        let (rec, key) = self.build_record(
            &job.request_id,
            job.invoked_at,
            job.failure,
            job.event,
            job.response,
            job.ctx,
        );
        let Ok(body) = to_json_bytes(&rec) else {
            warn!(request_id, "failed to serialize record; dropping");
            return None;
        };
        // spill に残った時点でレコードは保全済み — inflight ステージを
        // 消してよい。spill 失敗時はステージを残し、init 時の
        // timeout 変換に救いを残す（PUT 成功でも消える）。
        if self.spill(&request_id, &body) {
            self.clear_inflight(&request_id);
        }
        Some(StagedRecord {
            request_id,
            key,
            body,
        })
    }

    /// SHUTDOWN 経路の第1段。タイムアウトレコードを組み立てて
    /// /tmp へ同期退避する。戻り値の StagedRecord を `commit_staged`
    /// で PUT する二段構えにし、フラッシュ予算が尽きても
    /// spill だけは全件残るようにする。
    /// `reason` は Extensions API の shutdownReason（timeout/failure/spindown）。
    /// failureType は SPEC の enum 3 値（handler_error/init_error/timeout）の
    /// 制約から常に `timeout` とし、「応答が返らないまま shutdown した」の意。
    /// 区別が必要な情報は errorContext.errorType（Timeout/Failure/Spindown）に写す。
    pub fn stage_timeout(&self, inv: &Invocation, reason: Option<&str>) -> Option<StagedRecord> {
        let ctx = ErrorContext {
            error_type: Some(shutdown_error_type(reason).to_string()),
            error_message: Some(format!(
                "function did not respond before execution environment shutdown (reason: {})",
                reason.unwrap_or("unknown")
            )),
            stack_trace: None,
        };
        let (rec, key) = self.build_record(
            &inv.request_id,
            inv.invoked_at,
            FailureType::Timeout,
            EventInput {
                value: Some(inv.event.clone()),
                encoding: inv.encoding,
            },
            None,
            ctx,
        );
        let Ok(body) = to_json_bytes(&rec) else {
            return None;
        };
        // 先にローカルへ落とす: PutObject がウィンドウに間に合わなくても
        // 実行環境の /tmp が同一 sandbox で再利用される場合に拾える。
        // spill 成功時のみ inflight ステージを消す — 失敗時は残して
        // init 時の timeout 変換に救いを残す（PUT 成功でも消える）。
        if self.spill(&inv.request_id, &body) {
            self.clear_inflight(&inv.request_id);
        }
        Some(StagedRecord {
            request_id: inv.request_id.clone(),
            key,
            body,
        })
    }

    /// `/next` で観測した呼び出しを /tmp へステージする。
    /// external extension（/opt/extensions 起動の別プロセス）が
    /// SHUTDOWN 時にこのファイルを読んで未完呼び出しを記録する。
    /// プロセス間共有にしか使わないため、失敗しても呼び出しに影響しない。
    pub fn stage_inflight(&self, inv: &Invocation) {
        if let Some(body) = inv.to_staged() {
            spill::write_inflight(Path::new(&self.cfg.spill_dir), &inv.request_id, &body);
        }
    }

    /// 呼び出し完了時にステージを消す。残ると未完の証跡として
    /// timeout 記録へ変換されてしまうため、完了した呼び出し分は必ず消す。
    pub fn clear_inflight(&self, request_id: &str) {
        spill::clear_inflight(Path::new(&self.cfg.spill_dir), request_id);
    }

    /// 残った inflight ステージを timeout レコードへ変換する。
    /// 「応答を返す前に環境が畳まれた呼び出し」の回収で、init 直後
    /// （前環境の残滓、reason 不明 → None）と external extension の
    /// SHUTDOWN フラッシュ（reason あり）から呼ぶ。
    /// 稼働中に呼ぶと進行中の呼び出しを未完と誤認するため禁。
    /// 変換後はステージを消す — レコードは spill json として残るので
    /// PUT に失敗しても recover_spills が拾う。
    pub async fn recover_inflights(&self, reason: Option<&str>) {
        let dir = Path::new(&self.cfg.spill_dir);
        for path in spill::pending_inflights(dir) {
            let Some(inv) = std::fs::read(&path)
                .ok()
                .and_then(|b| Invocation::from_staged(&b))
            else {
                // 読めない・パースできないステージは復旧不能 — 消す
                if std::fs::remove_file(&path).is_ok() {
                    warn!(path = %path.display(), "dropping unreadable inflight stage");
                }
                continue;
            };
            if let Some(job) = self.stage_timeout(&inv, reason) {
                self.commit_staged(&job).await;
                // 変換済みのステージは消す（stage_timeout 側でも
                // 正規名を消すが、ファイル名と埋め込み rid が
                // 食い違う場合に備えて path 側も消す）
                let _ = std::fs::remove_file(&path);
            } else {
                // 変換に失敗したステージは残し、次回 init の再試行に任せる
                warn!(request_id = %inv.request_id, "failed to stage timed-out record");
            }
        }
    }

    /// stage 済みのレコードを S3 へ PUT する（時間は flush budget で
    /// 打ち切る）。届いたら spill を消す（残すと次回 init で同一キーへ
    /// 冗長な PUT が走る）。失敗しても spill は残るため記録は保全される。
    pub async fn commit_staged(&self, job: &StagedRecord) {
        match tokio::time::timeout(self.flush_budget(), self.put(&job.key, job.body.clone())).await
        {
            Ok(Ok(())) => {
                info!(request_id = %job.request_id, key = %job.key, "failure record saved");
                let spill_path =
                    Path::new(&self.cfg.spill_dir).join(spill::filename(&job.request_id));
                let _ = std::fs::remove_file(spill_path);
                // S3 に届いたのでローカルの証跡は全部消す
                // （spill 失敗で残った .inflight もここで拾う）
                self.clear_inflight(&job.request_id);
            }
            Ok(Err(e)) => {
                warn!(request_id = %job.request_id, error = %e, "s3 put failed; spill kept")
            }
            Err(_) => {
                warn!(request_id = %job.request_id, "s3 put exceeded budget; spill kept")
            }
        }
    }

    fn build_record(
        &self,
        request_id: &str,
        invoked_at: OffsetDateTime,
        failure: FailureType,
        event: EventInput,
        response: Option<Value>,
        ctx: ErrorContext,
    ) -> (FailureRecord, String) {
        let mut report = ScrubReport::default();
        let has_event = event.value.is_some();
        let event_is_raw = event.encoding == EventEncoding::RawText && has_event;
        let event_is_b64 = event.encoding == EventEncoding::Base64 && has_event;
        let mut event_v = event.value.unwrap_or(Value::Null);
        // Base64 イベントは opaque なので scrub しない。
        // base64 alphabet 上の数字列等がパターンに偶然一致すると
        // 置換で元バイト列を壊し、replay が別ペイロードを送ってしまう。
        // （base64 内の PII はそもそも検出不能で、実害は誤爆のみ）
        if !event_is_b64 {
            self.scrubber.scrub(&mut event_v, &mut report);
        }
        let mut response_v = response;
        if let Some(r) = response_v.as_mut() {
            self.scrubber.scrub(r, &mut report);
        }
        // errorMessage/stackTrace は自由テキストで、PII を含みうる。
        // event/response と同じ scrubReport に集約して証跡化する。
        let mut ctx = ctx;
        for f in [
            ctx.error_type.as_mut(),
            ctx.error_message.as_mut(),
            ctx.stack_trace.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            *f = self.scrubber.scrub_text(f, &mut report);
        }
        let (event_v, truncated) = truncate_event(&event_v, self.cfg.max_event_kb);
        let rec = FailureRecord {
            version: RECORD_VERSION,
            function_name: self.function_name.clone(),
            function_version: self.function_version.clone(),
            request_id: request_id.to_string(),
            invoked_at: crate::record::format_ts(&invoked_at),
            failure_type: failure.as_str(),
            event: event_v,
            response: response_v,
            error_context: ctx,
            event_is_raw_text: event_is_raw,
            event_is_base64: event_is_b64,
            scrub_report: ScrubReportJson {
                fields_redacted: report.fields_redacted,
                patterns_applied: report.patterns_applied.into_iter().collect(),
            },
            truncated,
        };
        let key = s3_key(&self.function_name, &invoked_at, request_id);
        (rec, key)
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let mut req = self
            .s3
            .put_object()
            .bucket(&self.cfg.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .content_type("application/json");
        req = match &self.cfg.kms_key {
            Some(k) => req
                .server_side_encryption(ServerSideEncryption::AwsKms)
                .ssekms_key_id(k.clone()),
            None => req.server_side_encryption(ServerSideEncryption::Aes256),
        };
        req.send()
            .await
            .map(|_| ())
            .map_err(|e| ZankyoError::Aws(e.to_string()))
    }

    /// 起動時に前回残った spill を再送する。spill ファイルは record JSON
    /// 本体なので、そこから functionName/invokedAt/requestId を読み
    /// 元の S3 キーを再構成する。送れたものだけ削除するため冪等に再実行できる。
    pub async fn recover_spills(&self) {
        let dir = Path::new(&self.cfg.spill_dir);
        let max_age = Duration::from_secs(self.cfg.spill_max_age_secs);
        for (path, body, key) in spill::pending(dir, max_age) {
            match tokio::time::timeout(self.put_timeout(), self.put(&key, body)).await {
                Ok(Ok(())) => {
                    let _ = std::fs::remove_file(&path);
                    info!(path = %path.display(), key, "recovered spilled record");
                }
                // 個別の PUT 失敗（当該キー固有の権限不足等）で残り全件を
                // 試行しないと、1 件の壊れたレコードが後続を塞ぎ続ける。
                Ok(Err(e)) => {
                    warn!(path = %path.display(), error = %e, "spill record PUT failed; trying rest")
                }
                // タイムアウトは S3 不通の可能性が高く、残りも同じ結果に
                // なる見込みが強い。次の再送周期に持ち越す。
                Err(_) => {
                    warn!(path = %path.display(), "spill PUT timed out; deferring rest");
                    break;
                }
            }
        }
        // 再送を試みた後で上限を適用する。先に絞ると、届くはずだった
        // 古いレコードを試行すらせず捨ててしまう。
        spill::enforce_cap(dir, self.cfg.spill_max_files);
        // 全件回収できたらディレクトリごと消し、rerun 後に残滓を残さない
        // （非空なら remove_dir は失敗するだけなので無害）。
        let _ = std::fs::remove_dir(dir);
    }

    fn spill(&self, request_id: &str, body: &[u8]) -> bool {
        spill::write(
            &self.cfg.spill_dir,
            self.cfg.spill_max_files,
            request_id,
            body,
        )
    }
}

/// Extensions API の shutdownReason を記録上の errorType へ写す。
fn shutdown_error_type(reason: Option<&str>) -> &'static str {
    match reason {
        Some("timeout") => "Timeout",
        Some("failure") => "Failure",
        Some("spindown") => "Spindown",
        _ => "Shutdown",
    }
}
