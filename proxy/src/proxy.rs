//! Runtime API プロキシ。
//!
//! 子プロセス（実ランタイム）から見ると zankyo が Runtime API 本体に
//! 見える。`/next`・`/response`・`/error`・`/init/error` だけを解釈し、
//! それ以外のパスは一切触らず中継する（成功呼び出しの観測コストを
//! ゼロに近づけるため、ボディを読むのは失敗判定が必要な経路だけ）。

use crate::inflight::{InFlight, Invocation};
use crate::record::{
    error_context_from_body, init_request_id, response_error_context, ErrorContext, FailureType,
};
use crate::store::Recorder;
use crate::upstream::{collect_bounded, forward, plain, strip_hop_by_hop, CollectError};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::Value;
use std::convert::Infallible;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tracing::{debug, warn};

pub type BoxedBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;
pub type HttpClient = Client<HttpConnector, BoxedBody>;

/// accept 失敗時の再試行間隔。一時的な FD 枯渇等での busy loop を避ける。
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// 空 or バッファ済みボディを BoxedBody に揃える。
pub fn boxed_full<B: Into<Bytes>>(b: B) -> BoxedBody {
    Full::new(b.into())
        .map_err(|never: Infallible| match never {})
        .boxed()
}

pub fn new_client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build(HttpConnector::new())
}

/// 非同期で投げたレコード保存タスク。プロセス終了前に
/// orchestrate がドレインするため、fire-and-forget で捨てない。
pub type PendingSaves = tokio::sync::Mutex<tokio::task::JoinSet<()>>;

pub struct ProxyState {
    /// 本来の Runtime API（`AWS_LAMBDA_RUNTIME_API` 原本の host:port）。
    pub upstream: String,
    pub client: HttpClient,
    pub inflight: Arc<InFlight>,
    pub recorder: Arc<Recorder>,
    /// ボディ上限などの動作ノブ。env / SSM 由来の値をそのまま使う。
    pub cfg: crate::config::Config,
    /// 進行中の save タスク。子終了時に残っていれば drain される。
    pub pending: PendingSaves,
}

/// 接続を受け付け続けるサーバループ。ランタイムは /next のロングポーリングと
/// 応答 POST を同じ接続で再利用することがあるため http1 keep-alive を使う。
pub async fn serve(listener: TcpListener, state: Arc<ProxyState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let st = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req| {
                        let st = st.clone();
                        async move { Ok::<_, Infallible>(handle(req, &st).await) }
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                        debug!(error = %e, "runtime api connection closed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

async fn handle(req: Request<Incoming>, st: &ProxyState) -> Response<BoxedBody> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let path = parts.uri.path().to_string();
    let body_limit = st.cfg.max_body_kb.saturating_mul(1024);
    let body_bytes = match collect_bounded(body, body_limit).await {
        Ok(b) => b,
        Err(CollectError::TooLarge) => {
            return plain(StatusCode::PAYLOAD_TOO_LARGE, "zankyo: body too large");
        }
        Err(CollectError::Read(e)) => {
            warn!(error = %e, "failed to read runtime request body");
            return plain(
                StatusCode::BAD_GATEWAY,
                "zankyo: failed to read request body",
            );
        }
    };

    let segs: Vec<&str> = path.split('/').collect();
    match (parts.method.as_str(), segs.as_slice()) {
        ("GET", ["", "2018-06-01", "runtime", "invocation", "next"]) => {
            handle_next(
                st,
                &parts.method,
                &path_and_query,
                &parts.headers,
                body_bytes,
                body_limit,
            )
            .await
        }
        ("POST", ["", "2018-06-01", "runtime", "invocation", rid, "response"])
        | ("POST", ["", "2018-06-01", "runtime", "invocation", rid, "error"]) => {
            let is_error = segs.last() == Some(&"error");
            handle_completion(
                st,
                &parts.method,
                &path_and_query,
                &parts.headers,
                body_bytes,
                rid,
                is_error,
            )
            .await
        }
        ("POST", ["", "2018-06-01", "runtime", "init", "error"]) => {
            handle_init_error(
                st,
                &parts.method,
                &path_and_query,
                &parts.headers,
                body_bytes,
            )
            .await
        }
        _ => {
            // 対象外パス（/restore/next 等）は中継のみ
            forward_or_502(
                st,
                &parts.method,
                &path_and_query,
                &parts.headers,
                body_bytes,
                Some(std::time::Duration::from_millis(st.cfg.forward_timeout_ms)),
                "upstream",
            )
            .await
            .map(boxed_response)
            .unwrap_or_else(|r| *r)
        }
    }
}

/// 上流へ転送し、失敗時は warn + 502 応答に落とす共通経路。
/// `ctx` はログ上で経路を区別するための短いラベル。
/// Err 側は Box で返し、Result のメモリサイズを小さく保つ。
async fn forward_or_502(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
    timeout: Option<std::time::Duration>,
    ctx: &'static str,
) -> Result<Response<Incoming>, Box<Response<BoxedBody>>> {
    match forward(st, method, pq, headers, body, timeout).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            warn!(error = %e, path = %pq, ctx, "upstream forward failed");
            Err(Box::new(plain(
                StatusCode::BAD_GATEWAY,
                "zankyo: upstream unreachable",
            )))
        }
    }
}

/// `Recorder::save` へ渡すレコード一式。
struct SaveJob {
    request_id: String,
    invoked_at: OffsetDateTime,
    failure: FailureType,
    event: Option<Value>,
    response: Option<Value>,
    ctx: ErrorContext,
}

/// ランタイムへの応答を遅らせないよう、レコード保存は非同期で行う。
/// タスクは `pending` に積まれ、プロセス終了前に orchestrate が
/// ドレインする（投げっぱなしにすると init_error 等の記録が消える）。
async fn spawn_save(pending: &PendingSaves, recorder: Arc<Recorder>, job: SaveJob) {
    pending.lock().await.spawn(async move {
        recorder
            .save(
                &job.request_id,
                job.invoked_at,
                job.failure,
                job.event,
                job.response,
                job.ctx,
            )
            .await;
    });
}

/// `/next`: 応答ヘッダから requestId 等を取り、ボディ（イベント）を
/// in-flight に保持してからランタイムへ返す。
async fn handle_next(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
    body_limit: usize,
) -> Response<BoxedBody> {
    // /next の転送はランタイムのロングポーリングを壊さないよう無制限に待つ
    let resp = match forward_or_502(st, method, pq, headers, body, None, "invocation/next").await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let (mut parts, resp_body) = resp.into_parts();
    // 上流の応答ヘッダにも hop-by-hop 規則を適用してからランタイムへ返す
    strip_hop_by_hop(&mut parts.headers);
    let bytes = match collect_bounded(resp_body, body_limit).await {
        Ok(b) => b,
        Err(CollectError::TooLarge) => {
            warn!("next response body exceeded limit; forwarding is skipped");
            return plain(StatusCode::BAD_GATEWAY, "zankyo: upstream body too large");
        }
        Err(CollectError::Read(e)) => {
            warn!(error = %e, "failed to read next response");
            return plain(StatusCode::BAD_GATEWAY, "zankyo: upstream body error");
        }
    };
    if parts.status == StatusCode::OK {
        let request_id = parts
            .headers
            .get("lambda-runtime-aws-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !request_id.is_empty() {
            let event = serde_json::from_slice::<Value>(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
            st.inflight.insert(Invocation {
                request_id,
                event,
                invoked_at: OffsetDateTime::now_utc(),
            });
        }
    }
    Response::from_parts(parts, boxed_full(bytes))
}

/// `/response`（成功扱いだが errorType 含有は失敗）と `/error`（失敗）。
async fn handle_completion(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
    rid: &str,
    is_error: bool,
) -> Response<BoxedBody> {
    // 失敗文脈は転送前に確定させる（body を消費するため）
    let ctx = if is_error {
        let header_type = headers
            .get("lambda-runtime-function-error-type")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        Some(error_context_from_body(&body, header_type))
    } else {
        response_error_context(&body)
    };
    // in-flight から外すのは forward の成否に関わらず行う。
    // 失敗時に残すと、完了した呼び出しが shutdown で timeout として
    // 二重記録される（再配達されれば別 requestId で来る）。
    let inv = st.inflight.remove(rid);
    let resp = forward_or_502(
        st,
        method,
        pq,
        headers,
        body.clone(),
        Some(std::time::Duration::from_millis(st.cfg.forward_timeout_ms)),
        "completion",
    )
    .await;
    // 失敗文脈は転送の成否に関わらず記録する。/error を受け取った事実が
    // 証跡そのものであり、上流断で 502 を返す場合も捨てない。
    if let Some(ctx) = ctx {
        let (event, invoked_at, request_id) = match inv {
            Some(i) => (Some(i.event), i.invoked_at, i.request_id),
            None => (None, OffsetDateTime::now_utc(), rid.to_string()),
        };
        spawn_save(
            &st.pending,
            st.recorder.clone(),
            SaveJob {
                request_id,
                invoked_at,
                failure: FailureType::HandlerError,
                event,
                response: serde_json::from_slice::<Value>(&body).ok(),
                ctx,
            },
        )
        .await;
    }
    resp.map(boxed_response).unwrap_or_else(|r| *r)
}

/// `/init/error`: 対応するイベントが存在しないため event は null。
async fn handle_init_error(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response<BoxedBody> {
    let header_type = headers
        .get("lambda-runtime-function-error-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let ctx = error_context_from_body(&body, header_type);
    let resp = forward_or_502(
        st,
        method,
        pq,
        headers,
        body.clone(),
        Some(std::time::Duration::from_millis(st.cfg.forward_timeout_ms)),
        "init/error",
    )
    .await;
    // init error は転送できなくても記録する（このイベントは他経路では拾えない）
    let now = OffsetDateTime::now_utc();
    spawn_save(
        &st.pending,
        st.recorder.clone(),
        SaveJob {
            request_id: init_request_id(&now),
            invoked_at: now,
            failure: FailureType::InitError,
            event: None,
            response: None,
            ctx,
        },
    )
    .await;
    resp.map(boxed_response).unwrap_or_else(|r| *r)
}

/// 上流の応答をランタイムへ返す形へ整える。hop-by-hop ヘッダを落とし、
/// ボディをバッファ済みに揃える。
fn boxed_response(mut resp: Response<Incoming>) -> Response<BoxedBody> {
    strip_hop_by_hop(resp.headers_mut());
    resp.map(|b| b.boxed())
}

#[cfg(test)]
mod tests {
    #[test]
    fn route_matching_shapes() {
        let segs: Vec<&str> = "/2018-06-01/runtime/invocation/next".split('/').collect();
        assert!(matches!(
            segs.as_slice(),
            ["", "2018-06-01", "runtime", "invocation", "next"]
        ));
        let segs: Vec<&str> = "/2018-06-01/runtime/invocation/abc/error"
            .split('/')
            .collect();
        assert!(matches!(
            segs.as_slice(),
            ["", "2018-06-01", "runtime", "invocation", "abc", "error"]
        ));
    }
}
