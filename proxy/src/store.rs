//! 失敗レコードの永続化。S3 PutObject が本線、失敗時・SHUTDOWN 時は
//! /tmp への退避（spill）で取りこぼしを減らす。
//! このモジュールだけが S3/ファイルシステムに触れる。

use crate::config::Config;
use crate::error::{Result, ZankyoError};
use crate::inflight::Invocation;
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

    /// レコードを組み立てて保存する。scrub はここで一括適用し、
    /// event/response 両方のレポートを集約する。
    pub async fn save(
        &self,
        request_id: &str,
        invoked_at: OffsetDateTime,
        failure: FailureType,
        event: Option<Value>,
        response: Option<Value>,
        ctx: ErrorContext,
    ) {
        let (rec, key) = self.build_record(request_id, invoked_at, failure, event, response, ctx);
        let body = match to_json_bytes(&rec) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, request_id, "failed to serialize record; dropping");
                return;
            }
        };
        match tokio::time::timeout(self.put_timeout(), self.put(&key, body.clone())).await {
            Ok(Ok(())) => info!(request_id, key, "failure record saved"),
            Ok(Err(e)) => {
                warn!(request_id, error = %e, "s3 put failed; spilling to /tmp");
                self.spill(request_id, &body);
            }
            Err(_) => {
                warn!(request_id, "s3 put timed out; spilling to /tmp");
                self.spill(request_id, &body);
            }
        }
    }

    /// SHUTDOWN 経路。まず /tmp へ同期退避してから、
    /// 残りの shutdown ウィンドウ内で S3 を試す（ベストエフォート）。
    /// `reason` は Extensions API の shutdownReason（timeout/failure/spindown）。
    /// failureType は SPEC の enum 3 値（handler_error/init_error/timeout）の
    /// 制約から常に `timeout` とし、「応答が返らないまま shutdown した」の意。
    /// 区別が必要な情報は errorContext.errorType（Timeout/Failure/Spindown）に写す。
    pub async fn save_during_shutdown(&self, inv: Invocation, reason: Option<&str>) {
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
            Some(inv.event),
            None,
            ctx,
        );
        let Ok(body) = to_json_bytes(&rec) else {
            return;
        };
        // 先にローカルへ落とす: PutObject がウィンドウに間に合わなくても
        // 実行環境の /tmp が同一 sandbox で再利用される場合に拾える
        self.spill(&inv.request_id, &body);
        match tokio::time::timeout(self.flush_budget(), self.put(&key, body)).await {
            Ok(Ok(())) => {
                info!(request_id = %inv.request_id, key, "timeout record saved during shutdown");
                // S3 へ届いた spill は保険の役目を終えたので消す。
                // 残すと次回 init で同一キーへ冗長な PUT が走る。
                let spill_path =
                    Path::new(&self.cfg.spill_dir).join(spill::filename(&inv.request_id));
                let _ = std::fs::remove_file(spill_path);
            }
            Ok(Err(e)) => warn!(request_id = %inv.request_id, error = %e, "shutdown flush failed"),
            Err(_) => warn!(request_id = %inv.request_id, "shutdown flush exceeded budget"),
        }
    }

    fn build_record(
        &self,
        request_id: &str,
        invoked_at: OffsetDateTime,
        failure: FailureType,
        event: Option<Value>,
        response: Option<Value>,
        ctx: ErrorContext,
    ) -> (FailureRecord, String) {
        let mut report = ScrubReport::default();
        let mut event_v = event.unwrap_or(Value::Null);
        self.scrubber.scrub(&mut event_v, &mut report);
        let mut response_v = response;
        if let Some(r) = response_v.as_mut() {
            self.scrubber.scrub(r, &mut report);
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
        for (path, body, key) in spill::pending(dir) {
            match tokio::time::timeout(self.put_timeout(), self.put(&key, body)).await {
                Ok(Ok(())) => {
                    let _ = std::fs::remove_file(&path);
                    info!(path = %path.display(), key, "recovered spilled record");
                }
                // S3 が届かない状態なら残りも同じ。次の再送に持ち越す
                _ => break,
            }
        }
        // 再送を試みた後で上限を適用する。先に絞ると、届くはずだった
        // 古いレコードを試行すらせず捨ててしまう。
        spill::enforce_cap(dir, self.cfg.spill_max_files);
    }

    fn spill(&self, request_id: &str, body: &[u8]) {
        spill::write(
            &self.cfg.spill_dir,
            self.cfg.spill_max_files,
            request_id,
            body,
        );
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
