//! Extensions API クライアント。
//! 単一バイナリが proxy と external extension を兼務するため、
//! `/register` で INVOKE/SHUTDOWN を購読し、`/event/next` をポーリングする。
//! SHUTDOWN（reason=timeout）を受けたら in-flight 呼び出しをフラッシュする。

use crate::error::{Result, ZankyoError};
use crate::inflight::InFlight;
use crate::proxy::{boxed_full, HttpClient};
use crate::store::Recorder;
use http::{Method, Request};
use http_body_util::BodyExt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const EXT_BASE: &str = "/2020-01-01/extension";
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// event/next が切れたときの再ポーリング間隔
const RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize)]
pub struct ExtensionEvent {
    #[serde(rename = "eventType")]
    pub event_type: String,
    #[serde(rename = "shutdownReason")]
    pub shutdown_reason: Option<String>,
    /// SHUTDOWN 時に実行環境が凍結される時刻（epoch ms）。
    /// フラッシュ予算の上限算出に使う。
    #[serde(rename = "deadlineMs")]
    pub deadline_ms: Option<i64>,
}

/// `/extension/register`。成功すると extension identifier が返る。
/// 登録に失敗しても proxy 機能は残るので、呼び出し側は warn のみで続行する。
pub async fn register(client: &HttpClient, upstream_api: &str) -> Result<String> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{upstream_api}{EXT_BASE}/register"))
        .header("Lambda-Extension-Name", "zankyo")
        .header("content-type", "application/json")
        .body(boxed_full(r#"{"events":["INVOKE","SHUTDOWN"]}"#))?;
    let resp = tokio::time::timeout(REGISTER_TIMEOUT, client.request(req))
        .await
        .map_err(|_| ZankyoError::Upstream("extension register timed out".into()))?
        .map_err(|e| ZankyoError::Upstream(e.to_string()))?;
    resp.headers()
        .get("lambda-extension-identifier")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            ZankyoError::Upstream("extension register: missing identifier header".into())
        })
}

/// Extensions API のイベントボディ上限。INVOKE/SHUTDOWN 通知は
/// 数百バイトのメタデータだけなので 1MiB あれば十分。
const EVENT_BODY_LIMIT: usize = 1024 * 1024;

/// `/event/next` はイベント到着までブロックするロングポーリング。
/// タイムアウトを付けない（イベントなし＝正常な待機）。
/// ボディは異常なサイズを読まないよう上限付きで読む。
pub async fn next_event(
    client: &HttpClient,
    upstream_api: &str,
    ext_id: &str,
) -> Result<ExtensionEvent> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{upstream_api}{EXT_BASE}/event/next"))
        .header("Lambda-Extension-Identifier", ext_id)
        .body(boxed_full(bytes::Bytes::new()))?;
    let resp = client
        .request(req)
        .await
        .map_err(|e| ZankyoError::Upstream(e.to_string()))?;
    let body = http_body_util::Limited::new(resp.into_body(), EVENT_BODY_LIMIT)
        .collect()
        .await
        .map_err(|e| ZankyoError::Upstream(format!("event body read failed: {e}")))?
        .to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

/// INVOKE/SHUTDOWN を受け取り続けるループ。
/// SHUTDOWN を受けたら in-flight 呼び出しをフラッシュして戻る。
/// INVOKE の requestId 相関は proxy 側の観測で完結するため、ここでは
/// SHUTDOWN 検知のみを担う。
pub async fn run_event_loop(
    client: HttpClient,
    upstream_api: String,
    ext_id: String,
    inflight: Arc<InFlight>,
    recorder: Arc<Recorder>,
) {
    loop {
        match next_event(&client, &upstream_api, &ext_id).await {
            Ok(ev) if ev.event_type == "SHUTDOWN" => {
                info!(
                    reason = ev.shutdown_reason.as_deref().unwrap_or("unknown"),
                    pending = inflight.len(),
                    "shutdown received; flushing in-flight invocations"
                );
                let pending = inflight.drain();
                if pending.is_empty() {
                    return;
                }
                // フラッシュ予算は設定値と、イベントが示す凍結期限の残時間の小さい方。
                // deadlineMs が来ない環境では設定値のみで判断する。
                let budget = flush_budget_for(ev.deadline_ms, recorder.flush_budget());
                let flush = async {
                    for inv in pending {
                        recorder.save_during_shutdown(inv).await;
                    }
                };
                if tokio::time::timeout(budget, flush).await.is_err() {
                    warn!("shutdown flush exceeded total budget");
                }
                return;
            }
            Ok(_) => continue,
            Err(e) => {
                // ネットワーク断・ボディ破損など。ポーリングを諦めると
                // timeout 捕捉を失うので、短い待機を挟んで再試行する。
                warn!(error = %e, "extension event poll failed; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// SHUTDOWN イベントの deadlineMs と設定予算から実際のフラッシュ予算を決める。
/// 凍結期限の 200ms 手前までを残時間とみなし、設定予算との小さい方を取る。
/// deadlineMs が無い・過去・負値のときは設定予算をそのまま使う。
fn flush_budget_for(deadline_ms: Option<i64>, configured: Duration) -> Duration {
    deadline_ms
        .and_then(|d| {
            let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
            u64::try_from(d - now_ms - 200).ok()
        })
        .map(|ms| configured.min(Duration::from_millis(ms)))
        .unwrap_or(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_without_deadline_uses_configured() {
        let cfg = Duration::from_millis(1200);
        assert_eq!(flush_budget_for(None, cfg), cfg);
    }

    #[test]
    fn budget_is_capped_by_configured() {
        let cfg = Duration::from_millis(1200);
        // 凍結期限が 60 秒先なら残時間より設定予算の方が小さい
        let far = time::OffsetDateTime::now_utc().unix_timestamp() * 1000 + 60_000;
        assert_eq!(flush_budget_for(Some(far), cfg), cfg);
    }

    #[test]
    fn budget_shrinks_to_remaining_window() {
        let cfg = Duration::from_millis(1200);
        // 凍結期限が 700ms 先 → 残 500ms 程度になるはず
        let near = time::OffsetDateTime::now_utc().unix_timestamp() * 1000 + 700;
        let b = flush_budget_for(Some(near), cfg);
        assert!(b < cfg && b <= Duration::from_millis(500));
    }

    #[test]
    fn past_deadline_falls_back_to_configured() {
        let cfg = Duration::from_millis(1200);
        // 残時間が負 → u64 変換に失敗するので設定予算へ倒れる
        let past = time::OffsetDateTime::now_utc().unix_timestamp() * 1000 - 1000;
        assert_eq!(flush_budget_for(Some(past), cfg), cfg);
    }

    #[test]
    fn shutdown_event_parses() {
        let ev: ExtensionEvent = serde_json::from_str(
            r#"{"eventType":"SHUTDOWN","shutdownReason":"timeout","deadlineMs":1}"#,
        )
        .unwrap();
        assert_eq!(ev.event_type, "SHUTDOWN");
        assert_eq!(ev.shutdown_reason.as_deref(), Some("timeout"));
        assert_eq!(ev.deadline_ms, Some(1));
    }
}
