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

/// `/event/next` はイベント到着までブロックするロングポーリング。
/// タイムアウトを付けない（イベントなし＝正常な待機）。
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
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(ZankyoError::Hyper)?
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
                let budget = ev
                    .deadline_ms
                    .and_then(|d| {
                        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp() * 1000;
                        let remaining = d - now_ms - 200; // 200ms は送り出しの安全マージン
                        u64::try_from(remaining).ok()
                    })
                    .map(|ms| recorder.flush_budget().min(Duration::from_millis(ms)))
                    .unwrap_or_else(|| recorder.flush_budget());
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
