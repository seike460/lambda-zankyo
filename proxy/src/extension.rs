//! Extensions API クライアント。
//! 単一バイナリが proxy と external extension を兼務するため、
//! `/register` で INVOKE/SHUTDOWN（記録しない場合は SHUTDOWN だけ）を購読し、
//! `/event/next` をポーリングする。
//! SHUTDOWN を受けたら、reason を問わず in-flight 呼び出しを timeout レコードとして
//! フラッシュする。
//!
//! SHUTDOWN の前に終了した extension は、終了コードを問わず Extension.Crash
//! になる（Init 中なら Init の失敗。
//! <https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html>）。
//! そのため一時的な失敗は再試行し、終了するのは回復できない場合だけにする。
//! register と event/next の 500 は、公式の API リファレンスが「Container error.
//! Non-recoverable state. Extension should exit promptly.」と定める
//! （<https://docs.aws.amazon.com/lambda/latest/dg/runtimes-extensions-api.html>）。
//! 終了の前に `/extension/init/error`・`/extension/exit/error` は送らない。
//! どちらも登録で得る識別子が要り、終了する場面（登録の拒否・500・接続不能）では
//! API が使えないため。

use crate::config::{
    Config, DEFAULT_EXT_BODY_KB, DEFAULT_EXT_MAX_POLL_FAILURES, DEFAULT_EXT_RETRY_MS,
    DEFAULT_REGISTER_TIMEOUT_MS,
};
use crate::error::{Result, ZankyoError};
use crate::inflight::InFlight;
use crate::proxy::{boxed_full, HttpClient};
use crate::store::Recorder;
use http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const EXT_BASE: &str = "/2020-01-01/extension";

/// Extensions API 呼び出しの失敗。再試行するかどうかを種類で決める。
#[derive(Debug)]
enum ApiError {
    /// 接続できなかった。続くなら Runtime API そのものが無い。
    Unreachable(String),
    /// Lambda がエラーのステータスを返した。
    Status(StatusCode, String),
    /// 途中での切断・ボディの破損・不正な JSON など。再試行で通りうる。
    Other(String),
}

impl ApiError {
    fn from_client(e: hyper_util::client::legacy::Error) -> Self {
        if e.is_connect() {
            Self::Unreachable(e.to_string())
        } else {
            Self::Other(e.to_string())
        }
    }

    /// AWS が「回復不能。速やかに終了すべき」と定める 500 か。
    fn is_container_error(&self) -> bool {
        matches!(self, Self::Status(s, _) if *s == StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "unreachable: {e}"),
            Self::Status(status, body) => write!(f, "status {status} body {body}"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

/// エラー応答のボディを診断用に読む（platform の拒否理由が分かる）。
async fn error_body(body: hyper::body::Incoming) -> String {
    http_body_util::Limited::new(body, 4096)
        .collect()
        .await
        .map(|c| String::from_utf8_lossy(&c.to_bytes()).into_owned())
        .unwrap_or_default()
}

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
/// `timeout` は再試行を含めた登録全体の上限で、実行環境の初期化を遅らせないためのもの。
/// 一時的な失敗は `retry_delay` の間隔で再試行する（`register_events`）。
pub async fn register(
    client: &HttpClient,
    upstream_api: &str,
    timeout: Duration,
    retry_delay: Duration,
) -> Result<String> {
    register_events(
        client,
        upstream_api,
        timeout,
        retry_delay,
        &["INVOKE", "SHUTDOWN"],
    )
    .await
}

/// 登録を `timeout` の範囲で再試行する。Init は 10 秒が上限なので、
/// 一時的な失敗（接続断・想定外のステータス）で諦めると、Lambda は Init を
/// 失敗させる。4xx（登録の拒否）と 500（回復不能）は、同じ要求を送り直しても
/// 通らないので、すぐに諦める。1 回の要求の上限は残り時間とする —
/// 応答の無いまま打ち切った要求を送り直すと、二重登録で拒否されうるため。
/// 時刻の足し算はしない（巨大な設定値で Instant があふれて panic しないように）。
async fn register_events(
    client: &HttpClient,
    upstream_api: &str,
    timeout: Duration,
    retry_delay: Duration,
    events: &[&str],
) -> Result<String> {
    let started = tokio::time::Instant::now();
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        let remaining = timeout.saturating_sub(started.elapsed());
        let err = match register_once(client, upstream_api, remaining, events).await {
            Ok(id) => return Ok(id),
            Err(e) => e,
        };
        let rejected = matches!(&err, ApiError::Status(s, _) if s.is_client_error())
            || err.is_container_error();
        if rejected || timeout.saturating_sub(started.elapsed()) <= retry_delay {
            return Err(ZankyoError::Upstream(format!(
                "extension register failed after {attempts} attempt(s): {err}"
            )));
        }
        warn!(error = %err, attempts, "extension register failed; retrying");
        tokio::time::sleep(retry_delay).await;
    }
}

/// `/extension/register` を 1 回送る。
async fn register_once(
    client: &HttpClient,
    upstream_api: &str,
    timeout: Duration,
    events: &[&str],
) -> std::result::Result<String, ApiError> {
    let body = serde_json::to_vec(&serde_json::json!({ "events": events }))
        .map_err(|e| ApiError::Other(e.to_string()))?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{upstream_api}{EXT_BASE}/register"))
        .header("Lambda-Extension-Name", "zankyo")
        .header("content-type", "application/json")
        .body(boxed_full(body))
        .map_err(|e| ApiError::Other(e.to_string()))?;
    let resp = tokio::time::timeout(timeout, client.request(req))
        .await
        .map_err(|_| ApiError::Other("extension register timed out".into()))?
        .map_err(ApiError::from_client)?;
    let (parts, body) = resp.into_parts();
    if let Some(id) = parts
        .headers
        .get("lambda-extension-identifier")
        .and_then(|v| v.to_str().ok())
    {
        return Ok(id.to_string());
    }
    Err(ApiError::Status(parts.status, error_body(body).await))
}

/// `/event/next` はイベント到着までブロックするロングポーリング。
/// タイムアウトを付けない（イベントなし＝正常な待機。AWS も付けないよう求めている）。
/// ボディは異常なサイズを読まないよう `body_limit` バイトで切る。
async fn next_event(
    client: &HttpClient,
    upstream_api: &str,
    ext_id: &str,
    body_limit: usize,
) -> std::result::Result<ExtensionEvent, ApiError> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{upstream_api}{EXT_BASE}/event/next"))
        .header("Lambda-Extension-Identifier", ext_id)
        .body(boxed_full(bytes::Bytes::new()))
        .map_err(|e| ApiError::Other(e.to_string()))?;
    let resp = client.request(req).await.map_err(ApiError::from_client)?;
    let (parts, body) = resp.into_parts();
    if !parts.status.is_success() {
        return Err(ApiError::Status(parts.status, error_body(body).await));
    }
    let body = http_body_util::Limited::new(body, body_limit)
        .collect()
        .await
        .map_err(|e| ApiError::Other(format!("event body read failed: {e}")))?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|e| ApiError::Other(format!("invalid event: {e}")))
}

/// INVOKE/SHUTDOWN を受け取り続けるループ。
/// SHUTDOWN を受けたら in-flight 呼び出しをフラッシュして `true` で戻る。
/// Extensions API が回復不能になって諦めた場合は `false`（timeout 捕捉だけを失う
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
    let limits = PollLimits::from_config(recorder.config());
    let Some(ev) = wait_for_shutdown(&client, &upstream_api, &ext_id, &limits).await else {
        return false;
    };
    info!(
        reason = ev.shutdown_reason.as_deref().unwrap_or("unknown"),
        pending = inflight.len(),
        "shutdown received; flushing in-flight invocations"
    );
    // drain と記録権確保を原子的に行う — 隙間に到着した
    // /error が event 欠落のまま記録権を取る競合を防ぐ。
    // （/error が先ならイベント付き handler_error が記録され、
    // こちらは drain に残らないので両方ともイベントが残る）
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
    true
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
    let limits = PollLimits::from_config(cfg);
    let ext_id = match register(
        &client,
        &upstream_api,
        Duration::from_millis(cfg.register_timeout_ms),
        limits.retry_delay,
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
    let Some(ev) = wait_for_shutdown(&client, &upstream_api, &ext_id, &limits).await else {
        return false;
    };
    info!(
        reason = ev.shutdown_reason.as_deref().unwrap_or("unknown"),
        "shutdown received; converting staged inflights to timeout records"
    );
    let budget = flush_budget_for(ev.deadline_ms, recorder.flush_budget());
    let flush = recorder.recover_inflights(ev.shutdown_reason.as_deref());
    if tokio::time::timeout(budget, flush).await.is_err() {
        warn!("inflight flush exceeded shutdown budget; spills remain for next init");
    }
    true
}

/// 記録しない（passthrough の）external extension プロセス。
/// 登録前や SHUTDOWN 前に extension が終了すると、終了コードに
/// 関係なく platform は Extension.Crash として Init を失敗させる。
/// そのため SHUTDOWN だけを購読して環境の終了まで待つ（fail-open）。
/// INVOKE は購読しない — 呼び出しごとの往復を増やさないため。
/// 設定自体が壊れている場合もあるので、ノブは既定値を使う。
/// 戻り値は「SHUTDOWN を受けたか」。
pub async fn run_passthrough_agent(client: HttpClient, upstream_api: String) -> bool {
    let limits = PollLimits {
        body_limit: DEFAULT_EXT_BODY_KB * 1024,
        retry_delay: Duration::from_millis(DEFAULT_EXT_RETRY_MS),
        max_failures: DEFAULT_EXT_MAX_POLL_FAILURES,
    };
    let ext_id = match register_events(
        &client,
        &upstream_api,
        Duration::from_millis(DEFAULT_REGISTER_TIMEOUT_MS),
        limits.retry_delay,
        &["SHUTDOWN"],
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "external extension register failed");
            return false;
        }
    };
    info!("recording disabled; external extension idles until shutdown");
    wait_for_shutdown(&client, &upstream_api, &ext_id, &limits)
        .await
        .is_some()
}

/// register と `/event/next` ポーリングのノブ。
struct PollLimits {
    body_limit: usize,
    /// 失敗から次の試行までの間隔。
    retry_delay: Duration,
    /// Runtime API に接続できない状態が何回続いたら諦めるか。
    max_failures: u32,
}

impl PollLimits {
    fn from_config(cfg: &Config) -> Self {
        Self {
            body_limit: cfg.ext_body_kb.saturating_mul(1024),
            retry_delay: Duration::from_millis(cfg.ext_retry_ms),
            max_failures: cfg.ext_max_poll_failures,
        }
    }
}

/// SHUTDOWN が届くまで `/event/next` をポーリングする。INVOKE は読み捨てる。
/// SHUTDOWN の前に抜けると Extension.Crash になるため、失敗しても
/// `retry_delay` の間隔で再試行を続ける。諦めるのは次の 2 つだけ（None を返す）。
/// - Lambda が 500 を返した。AWS はこれを回復不能とし、速やかな終了を求めている。
/// - Runtime API に接続できない状態が `max_failures` 回続いた。SHUTDOWN を
///   送る相手がもう無い（無限リトライで zombie 化しない）。
///
/// 403 などは再試行する。公式 RIE（rapid）の実装では、前の `/event/next` の
/// 接続が切れた extension は待機中のままで、次のイベントを配るまで
/// `/event/next` を 403 で断られる。
/// 警告は連続失敗の 1・2・4・8… 回目だけに出し、ログを溢れさせない。
async fn wait_for_shutdown(
    client: &HttpClient,
    upstream_api: &str,
    ext_id: &str,
    limits: &PollLimits,
) -> Option<ExtensionEvent> {
    let mut failures: u32 = 0;
    let mut unreachable: u32 = 0;
    loop {
        let err = match next_event(client, upstream_api, ext_id, limits.body_limit).await {
            Ok(ev) if ev.event_type == "SHUTDOWN" => return Some(ev),
            Ok(_) => {
                failures = 0;
                unreachable = 0;
                continue;
            }
            Err(e) => e,
        };
        if err.is_container_error() {
            warn!(error = %err, "extensions API reported a non-recoverable error; exiting");
            return None;
        }
        failures = failures.saturating_add(1);
        unreachable = match err {
            ApiError::Unreachable(_) => unreachable + 1,
            _ => 0,
        };
        if unreachable >= limits.max_failures {
            warn!(
                failures = unreachable,
                "extensions API is unreachable; giving up"
            );
            return None;
        }
        if failures.is_power_of_two() {
            warn!(error = %err, failures, "extension event poll failed; retrying");
        }
        tokio::time::sleep(limits.retry_delay).await;
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
        let cfg = Duration::from_millis(10_000);
        // 凍結期限が 5 秒先 → 残 4.8 秒以下になるはず。実時計に依存するため、
        // テスト中にスレッドが止まっても期限を過ぎないよう数秒の幅を取る
        let now_ms = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        let near = (now_ms + 5_000) as i64;
        let b = flush_budget_for(Some(near), cfg);
        assert!(b < cfg && b <= Duration::from_millis(4_800));
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

    #[tokio::test]
    async fn poll_gives_up_when_runtime_api_is_gone() {
        // 閉じたポートには接続できない。Runtime API が無いとみなし、
        // 上限の回数だけ試して諦める（接続できない相手を待ち続けない）
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let limits = PollLimits {
            body_limit: 1024,
            retry_delay: Duration::from_millis(1),
            max_failures: 3,
        };
        let polled = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_shutdown(&crate::proxy::new_client(), &addr, "ext-id", &limits),
        )
        .await
        .expect("must give up once the runtime API is unreachable");
        assert!(polled.is_none());
    }
}
