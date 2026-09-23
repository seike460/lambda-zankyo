//! 失敗レコードの永続化。S3 PutObject が本線、失敗時・SHUTDOWN 時は
//! /tmp への退避（spill）で取りこぼしを減らす。
//! このモジュールだけが S3/ファイルシステムに触れる。

use crate::config::Config;
use crate::error::{Result, ZankyoError};
use crate::inflight::Invocation;
use crate::record::{
    s3_key, s3_key_from_parts, to_json_bytes, truncate_event, ErrorContext, FailureRecord,
    FailureType, ScrubReportJson, RECORD_VERSION,
};
use crate::scrub::{ScrubReport, Scrubber};
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
                    Path::new(&self.cfg.spill_dir).join(spill_filename(&inv.request_id));
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
        // 前回までの蓄積が上限を超えていても、新規 spill を待たずに先に絞る
        enforce_spill_cap(dir, self.cfg.spill_max_files);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // ディレクトリ自体が無い = 退避なし
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(body) = std::fs::read(&path) else {
                continue;
            };
            // 壊れた JSON や必須フィールド欠落は永遠に復旧できない。
            // 残すと rerun のたびに積み上がる stale 残滓になるので捨てる。
            let Some(key) = key_for_spilled(&body) else {
                if std::fs::remove_file(&path).is_ok() {
                    warn!(path = %path.display(), "dropping unrecoverable spilled file");
                }
                continue;
            };
            match tokio::time::timeout(self.put_timeout(), self.put(&key, body)).await {
                Ok(Ok(())) => {
                    let _ = std::fs::remove_file(&path);
                    info!(path = %path.display(), key, "recovered spilled record");
                }
                // S3 が届かない状態なら残りも同じ。次の init に持ち越す
                _ => break,
            }
        }
    }

    fn spill(&self, request_id: &str, body: &[u8]) {
        let dir = &self.cfg.spill_dir;
        let path = Path::new(dir).join(spill_filename(request_id));
        let result = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, body));
        match result {
            Ok(()) => {
                info!(request_id, path = %path.display(), "record spilled to /tmp");
                enforce_spill_cap(Path::new(dir), self.cfg.spill_max_files);
            }
            Err(e) => warn!(request_id, error = %e, "failed to spill record"),
        }
    }
}

/// spill dir の JSON ファイル数を `cap` 以下に抑える。
/// S3 が届かない状態が続いても /tmp を使い尽くさないよう、
/// 更新時刻の古いものから捨てる（新しい記録ほど復旧価値が高い前提）。
fn enforce_spill_cap(dir: &Path, cap: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .map(|p| {
            let mtime = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    if files.len() <= cap {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    for (_, path) in files.iter().take(files.len() - cap) {
        if std::fs::remove_file(path).is_ok() {
            warn!(path = %path.display(), "spill cap reached; dropping oldest record");
        }
    }
}

/// requestId は外部入力（Runtime API ヘッダ）由来なので、ファイル名に
/// 使える文字だけへ正規化する。`/` や `..` を含む値で spill_dir の
/// 外へ書き出さないための防御。
fn spill_filename(request_id: &str) -> String {
    let clean: String = request_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.');
    let name = if clean.is_empty() { "record" } else { clean };
    format!("{name}.json")
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

/// spill した record JSON から S3 キーを再構成する。
/// invokedAt は `yyyy-mm-ddTHH:MM:SSZ` 固定長なので日付部分だけ切り出す。
/// レイアウト本体は record::s3_key_from_parts と共有する。
fn key_for_spilled(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let function = v.get("functionName")?.as_str()?;
    let request_id = v.get("requestId")?.as_str()?;
    let invoked_at = v.get("invokedAt")?.as_str()?;
    let (y, m, d) = (
        invoked_at.get(0..4)?.parse::<i32>().ok()?,
        invoked_at.get(5..7)?.parse::<u8>().ok()?,
        invoked_at.get(8..10)?.parse::<u8>().ok()?,
    );
    Some(s3_key_from_parts(function, y, m, d, request_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_spilled_rebuilds_layout() {
        let body = br#"{"functionName":"fn","requestId":"r1","invokedAt":"2026-09-22T01:02:03Z"}"#;
        assert_eq!(
            key_for_spilled(body),
            Some("zankyo/fn/2026/09/22/r1.json".to_string())
        );
    }

    #[test]
    fn key_for_spilled_rejects_malformed() {
        assert_eq!(key_for_spilled(b"not json"), None);
        assert_eq!(key_for_spilled(br#"{"functionName":"fn"}"#), None);
    }

    #[test]
    fn spill_filename_strips_path_separators() {
        assert_eq!(spill_filename("req-123"), "req-123.json");
        // `.` `/` `\` は全て `_` へ潰れるので traversal できない
        assert_eq!(spill_filename("../../etc/passwd"), "______etc_passwd.json");
        assert_eq!(spill_filename("a/b\\c"), "a_b_c.json");
        assert_eq!(spill_filename(""), "record.json");
        assert_eq!(spill_filename("../.."), "_____.json");
    }

    #[test]
    fn spill_cap_drops_oldest_files() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("zankyo-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 古→新の順に 3 ファイル、mtime を明示して順序を確定させる
        for i in 0..3u64 {
            let p = dir.join(format!("f{i}.json"));
            std::fs::write(&p, b"{}").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(i + 1))
                .unwrap();
        }
        enforce_spill_cap(&dir, 2);
        // 最古の f0 だけが消え、新しい 2 つが残る
        assert!(!dir.join("f0.json").exists());
        assert!(dir.join("f1.json").exists());
        assert!(dir.join("f2.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
