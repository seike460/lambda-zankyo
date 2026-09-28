//! 失敗レコードの形式と組み立て。S3 キー設計・エラー文脈の抽出もここ。
//! serde で wire format と Rust 型を一致させ、CLI 側 (cli/src/record.ts) と
//! 同じスキーマを共有する。

use crate::error::{Result, ZankyoError};
use serde::Serialize;
use serde_json::Value;
use time::macros::format_description;
use time::OffsetDateTime;

pub const RECORD_VERSION: &str = "1";
/// S3 キーの先頭セグメント。CLI (cli/src/record.ts)・CDK construct の
/// IAM スコープ (`zankyo/*`) と揃える規約。
pub const KEY_PREFIX: &str = "zankyo";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureType {
    /// `/error` または `/response` 内の errorType 含有。
    HandlerError,
    /// `/init/error`。イベントは存在しない。
    InitError,
    /// 応答が返らないまま実行環境が畳まれた in-flight 呼び出し。
    /// 関数のタイムアウトに限らない（理由は errorContext.errorType）。
    Timeout,
}

impl FailureType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HandlerError => "handler_error",
            Self::InitError => "init_error",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct ErrorContext {
    #[serde(rename = "errorType", skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(rename = "errorMessage", skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "stackTrace", skip_serializing_if = "Option::is_none")]
    pub stack_trace: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScrubReportJson {
    #[serde(rename = "fieldsRedacted")]
    pub fields_redacted: usize,
    #[serde(rename = "patternsApplied")]
    pub patterns_applied: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FailureRecord {
    pub version: &'static str,
    #[serde(rename = "functionName")]
    pub function_name: String,
    #[serde(rename = "functionVersion")]
    pub function_version: String,
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// RFC3339 (UTC)。イベントを受け取った側の時刻。
    #[serde(rename = "invokedAt")]
    pub invoked_at: String,
    #[serde(rename = "failureType")]
    pub failure_type: &'static str,
    pub event: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    #[serde(rename = "errorContext")]
    pub error_context: ErrorContext,
    /// イベントが非 JSON ボディの生テキストで保存されている場合 true。
    /// CLI は replay/redrive 時に JSON 再エンコードせず原文を送る。
    #[serde(rename = "eventIsRawText", skip_serializing_if = "is_false")]
    pub event_is_raw_text: bool,
    /// イベントが UTF-8 でないバイナリの場合 true。
    /// `event` は base64 文字列として保持され、CLI は replay 時に
    /// デコードして元のバイト列を再送する。
    #[serde(rename = "eventIsBase64", skip_serializing_if = "is_false")]
    pub event_is_base64: bool,
    #[serde(rename = "scrubReport")]
    pub scrub_report: ScrubReportJson,
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

const TS_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

pub fn format_ts(t: &OffsetDateTime) -> String {
    t.format(TS_FORMAT)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// `zankyo/{function}/{yyyy}/{mm}/{dd}/{requestId}.json`
pub fn s3_key(function_name: &str, invoked_at: &OffsetDateTime, request_id: &str) -> String {
    s3_key_from_parts(
        function_name,
        invoked_at.year(),
        u8::from(invoked_at.month()),
        invoked_at.day(),
        request_id,
    )
}

/// キー設計の唯一の実装。spill 復旧など文字列日付しか持たない経路も
/// ここを通るため、レイアウト変更は 1 箇所で済む。
pub fn s3_key_from_parts(
    function_name: &str,
    year: i32,
    month: u8,
    day: u8,
    request_id: &str,
) -> String {
    format!("{KEY_PREFIX}/{function_name}/{year:04}/{month:02}/{day:02}/{request_id}.json")
}

/// イベントが上限を超える場合、先頭 N KB のみ残す。
/// fixture 再現性より「イベントの断片が残ること」を優先する（SPEC 決定事項）。
/// JSON 構造は保てなくなるため文字列として格納し `truncated: true` を付ける。
pub fn truncate_event(event: &Value, max_kb: usize) -> (Value, bool) {
    let limit = max_kb.saturating_mul(1024);
    let rendered = match event {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if rendered.len() <= limit {
        return (event.clone(), false);
    }
    let mut end = limit;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    (Value::String(rendered[..end].to_string()), true)
}

/// `/error`・`/init/error` のボディからエラー文脈を取る。
/// ランタイムが付ける `Lambda-Runtime-Function-Error-Type` ヘッダがあれば
/// ボディ側の欠落を補う。
pub fn error_context_from_body(body: &[u8], header_error_type: Option<String>) -> ErrorContext {
    let mut ctx = ErrorContext {
        error_type: header_error_type,
        ..Default::default()
    };
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(t) = v.get("errorType").and_then(|x| x.as_str()) {
            ctx.error_type = Some(t.to_string());
        }
        if let Some(m) = v.get("errorMessage").and_then(|x| x.as_str()) {
            ctx.error_message = Some(m.to_string());
        }
        // Node.js ランタイムはスタックを `trace` キーで送る
        match v.get("stackTrace").or_else(|| v.get("trace")) {
            Some(Value::String(s)) => ctx.stack_trace = Some(s.clone()),
            Some(Value::Array(frames)) => {
                ctx.stack_trace = Some(
                    frames
                        .iter()
                        .filter_map(|f| f.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            _ => {}
        }
    } else {
        // JSON でないエラーボディもそのまま残す（情報を落とさない）
        let text = String::from_utf8_lossy(body);
        if !text.trim().is_empty() {
            ctx.error_message = Some(text.into_owned());
        }
    }
    ctx
}

/// `/response` に流れたボディがエラー形か判定する。
/// SPEC は「errorType 含有」を失敗と定める。errorMessage 単独で
/// 発火すると、正常応答にエラー形フィールドを返す API（GraphQL 等）を
/// 失敗として誤記録するため、errorType の非空文字列だけを見る
/// （`"errorType": null` を正常応答に載せる API もある）。
pub fn response_error_context(body: &[u8]) -> Option<ErrorContext> {
    let v = serde_json::from_slice::<Value>(body).ok()?;
    let obj = v.as_object()?;
    match obj.get("errorType").and_then(|t| t.as_str()) {
        Some(t) if !t.is_empty() => Some(error_context_from_body(body, None)),
        _ => None,
    }
}

/// init error 用の擬似 requestId（実リクエストが存在しないため時刻由来）。
/// nanos まで使い、同一ミリ秒の init リトライで S3 キーが衝突しないようにする。
pub fn init_request_id(at: &OffsetDateTime) -> String {
    format!("init-{}", at.unix_timestamp_nanos())
}

pub fn to_json_bytes(rec: &FailureRecord) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(rec).map_err(ZankyoError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    #[test]
    fn s3_key_layout() {
        let at = datetime!(2026-09-22 12:34:56 UTC);
        assert_eq!(
            s3_key("my-api", &at, "req-1"),
            "zankyo/my-api/2026/09/22/req-1.json"
        );
    }

    #[test]
    fn truncation_keeps_head_and_flags() {
        let big = json!({"data": "x".repeat(300_000)});
        let (v, truncated) = truncate_event(&big, 1); // 1KB
        assert!(truncated);
        let s = v.as_str().unwrap();
        assert!(s.len() <= 1024);
        assert!(s.starts_with("{\"data\":\"xxxx"));
    }

    #[test]
    fn small_event_is_untouched() {
        let small = json!({"a": 1});
        let (v, truncated) = truncate_event(&small, 256);
        assert!(!truncated);
        assert_eq!(v, small);
    }

    #[test]
    fn error_context_reads_lambda_shape() {
        let body = br#"{"errorType":"Error","errorMessage":"boom","stackTrace":["a","b"]}"#;
        let ctx = error_context_from_body(body, None);
        assert_eq!(ctx.error_type.as_deref(), Some("Error"));
        assert_eq!(ctx.error_message.as_deref(), Some("boom"));
        assert_eq!(ctx.stack_trace.as_deref(), Some("a\nb"));
    }

    #[test]
    fn error_context_reads_nodejs_trace() {
        let body = br#"{"errorType":"TypeError","errorMessage":"boom","trace":["TypeError: boom","    at handler (/var/task/index.js:3:9)"]}"#;
        let ctx = error_context_from_body(body, None);
        assert_eq!(ctx.error_type.as_deref(), Some("TypeError"));
        assert_eq!(
            ctx.stack_trace.as_deref(),
            Some("TypeError: boom\n    at handler (/var/task/index.js:3:9)")
        );
    }

    #[test]
    fn header_error_type_fills_gap() {
        let ctx = error_context_from_body(b"{}", Some("Runtime.ExitError".into()));
        assert_eq!(ctx.error_type.as_deref(), Some("Runtime.ExitError"));
    }

    #[test]
    fn non_json_body_becomes_message() {
        let ctx = error_context_from_body(b"segmentation fault", None);
        assert_eq!(ctx.error_message.as_deref(), Some("segmentation fault"));
    }

    #[test]
    fn response_error_requires_error_keys() {
        assert!(response_error_context(br#"{"errorType":"Error","errorMessage":"x"}"#).is_some());
        assert!(response_error_context(br#"{"ok":true,"count":3}"#).is_none());
        // null・空文字の errorType は失敗とみなさない（正常応答に含めうる）
        assert!(response_error_context(br#"{"errorType":null,"data":1}"#).is_none());
        assert!(response_error_context(br#"{"errorType":""}"#).is_none());
    }
}
