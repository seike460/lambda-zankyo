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
/// `timeout` は実行環境の初期化を遅らせないための上限。
pub async fn register(
    client: &HttpClient,
    upstream_api: &str,
    timeout: Duration,
) -> Result<String> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{upstream_api}{EXT_BASE}/register"))
        .header("Lambda-Extension-Name", "zankyo")
        .header("content-type", "application/json")
        .body(boxed_full(r#"{"events":["INVOKE","SHUTDOWN"]}"#))?;
    let resp = tokio::time::timeout(timeout, client.request(req))
        .await
        .map_err(|_| ZankyoError::Upstream("extension register timed out".into()))?
        .map_err(|e| ZankyoError::Upstream(e.to_string()))?;
    let (parts, body) = resp.into_parts();
    if let Some(id) = parts
        .headers
        .get("lambda-extension-identifier")
        .and_then(|v| v.to_str().ok())
    {
        return Ok(id.to_string());
    }
    // 失敗時はステータスとボディを残す（platform の拒否理由が分かる）。
    let body = http_body_util::Limited::new(body, 4096)
        .collect()
        .await
        .map(|c| String::from_utf8_lossy(&c.to_bytes()).into_owned())
        .unwrap_or_default();
    Err(ZankyoError::Upstream(format!(
        "extension register: status {} body {}",
        parts.status, body
    )))
}

/// `/event/next` はイベント到着までブロックするロングポーリング。
/// タイムアウトを付けない（イベントなし＝正常な待機）。
/// ボディは異常なサイズを読まないよう `body_limit` バイトで切る。
pub async fn next_event(
    client: &HttpClient,
    upstream_api: &str,
    ext_id: &str,
    body_limit: usize,
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
    let body = http_body_util::Limited::new(resp.into_body(), body_limit)
        .collect()
        .await
        .map_err(|e| ZankyoError::Upstream(format!("event body read failed: {e}")))?
        .to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

/// INVOKE/SHUTDOWN を受け取り続けるループ。
/// SHUTDOWN を受けたら in-flight 呼び出しをフラッシュして `true` で戻る。
/// ポーリング断が続いて諦めた場合は `false`（timeout 捕捉だけを失う
/// 縮退運転であり、関数本体の継続には影響しない）。
/// INVOKE の requestId 相関は proxy 側の観測で完結するため、ここでは
/// SHUTDOWN 検知のみを担う。
pub async fn run_event_loop(
    client: HttpClient,
    upstream_api: String,
    ext_id: String,
    inflight: Arc<InFlight>,
    recorder: Arc<Recorder>,
) -> bool {
    let cfg = recorder.config();
    let retry_delay = Duration::from_millis(cfg.ext_retry_ms);
    let max_failures = cfg.ext_max_poll_failures;
    let body_limit = cfg.ext_body_kb.saturating_mul(1024);
    let mut failures: u32 = 0;
    loop {
        match next_event(&client, &upstream_api, &ext_id, body_limit).await {
            Ok(ev) if ev.event_type == "SHUTDOWN" => {
                info!(
                    reason = ev.shutdown_reason.as_deref().unwrap_or("unknown"),
                    pending = inflight.len(),
                    "shutdown received; flushing in-flight invocations"
                );
                let pending = inflight.drain_and_claim();
                if pending.is_empty() {
                    return true;
                }
                // フラッシュ予算は設定値と、イベントが示す凍結期限の残時間の小さい方。
                // deadlineMs が来ない環境では設定値のみで判断する。
                let budget = flush_budget_for(ev.deadline_ms, recorder.flush_budget());
                let reason = ev.shutdown_reason.clone();
                // パス1: 全件を同期で /tmp へ退避する。最初の PUT が予算を
                // 食い潰すと 2 件目以降が spill すらされず消えるため、
                // 書き込みが速い spill を先に済ませてから PUT に入る。
                let mut jobs = Vec::new();
                // drain と記録権確保を原子的に行う — 隙間に到着した
                // /error が event 欠落のまま記録権を取る競合を防ぐ。
                // （/error が先ならイベント付き handler_error が記録され、
                // こちらは drain に残らないので両方ともイベントが残る）
                for inv in pending {
                    // None はシリアライズ失敗＝残せない。warn に残す。
                    if let Some(j) = recorder.stage_timeout(&inv, reason.as_deref()) {
                        jobs.push(j);
                    } else {
                        warn!(request_id = %inv.request_id, "failed to stage timed-out record");
                    }
                }
                // パス2: 残予算内で PUT。間に合わない分は spill が残り、
                // 次回 init の recover_spills が回収する。
                let flush = async {
                    for job in &jobs {
                        recorder.commit_staged(job).await;
                    }
                };
                if tokio::time::timeout(budget, flush).await.is_err() {
                    warn!("shutdown flush exceeded total budget");
                }
                return true;
            }
            Ok(_) => {
                failures = 0;
                continue;
            }
            Err(e) => {
                // ネットワーク断・ボディ破損など。ポーリングを諦めると
                // timeout 捕捉を失うので、短い待機を挟んで再試行する。
                // ただし連続失敗が上限を超えたら Extensions API の障害と
                // みなしてループを抜ける（無限リトライで zombie 化しない）。
                failures += 1;
                if failures >= max_failures {
                    warn!(failures, "extension event poll keeps failing; giving up");
                    return false;
                }
                warn!(error = %e, failures, "extension event poll failed; retrying");
                tokio::time::sleep(retry_delay).await;
            }
        }
    }
}

/// external extension プロセスのイベントループ。
/// `/opt/extensions/` から platform が直接起動した agent 用。
/// internal extension（exec wrapper 内 register）には SHUTDOWN が
/// 届かない AWS 仕様のため、タイムアウト捕捉はこちらが担う。
/// in-flight は別プロセスの proxy とメモリを共有できないため、
/// proxy が /next で書く `.inflight` ステージを読んで未完呼び出しを
/// timeout レコードへ変換する。
/// 戻り値は「SHUTDOWN を受けてフラッシュまで済ませたか」。
pub async fn run_agent(client: HttpClient, upstream_api: String, recorder: Arc<Recorder>) -> bool {
    let cfg = recorder.config();
    let retry_delay = Duration::from_millis(cfg.ext_retry_ms);
    let max_failures = cfg.ext_max_poll_failures;
    let body_limit = cfg.ext_body_kb.saturating_mul(1024);
    let ext_id = match register(
        &client,
        &upstream_api,
        Duration::from_millis(cfg.register_timeout_ms),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "external extension register failed");
            return false;
        }
    };
    info!("external extension registered; waiting for shutdown events");
    let mut failures: u32 = 0;
    loop {
        match next_event(&client, &upstream_api, &ext_id, body_limit).await {
            Ok(ev) if ev.event_type == "SHUTDOWN" => {
                info!(
                    reason = ev.shutdown_reason.as_deref().unwrap_or("unknown"),
                    "shutdown received; converting staged inflights to timeout records"
                );
                let budget = flush_budget_for(ev.deadline_ms, recorder.flush_budget());
                let flush = recorder.recover_inflights(ev.shutdown_reason.as_deref());
                if tokio::time::timeout(budget, flush).await.is_err() {
                    warn!("inflight flush exceeded shutdown budget; spills remain for next init");
                }
                return true;
            }
            Ok(_) => {
                failures = 0;
                continue;
            }
            Err(e) => {
                failures += 1;
                if failures >= max_failures {
                    warn!(failures, "extension event poll keeps failing; giving up");
                    return false;
                }
                warn!(error = %e, failures, "extension event poll failed; retrying");
                tokio::time::sleep(retry_delay).await;
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
            // nanos→ms の切り捨てで秒未満の丸め誤差を抑え、
            // i64 の減算は saturating でオーバーフローを防ぐ
            let now_ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
            let remaining = d
                .saturating_sub(i64::try_from(now_ms).unwrap_or(i64::MAX))
                .saturating_sub(200);
            u64::try_from(remaining).ok()
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
        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        let near = (now_ms + 700) as i64;
        let b = flush_budget_for(Some(near), cfg);
        assert!(b < cfg && b <= Duration::from_millis(500));
    }

    #[test]
    fn past_deadline_falls_back_to_configured() {
        let cfg = Duration::from_millis(1200);
        // 残時間が負 → u64 変換に失敗するので設定予算へ倒れる
        let past =
            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000 - 1000) as i64;
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
