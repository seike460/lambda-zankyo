//! Runtime API プロキシ。
//!
//! 子プロセス（実ランタイム）から見ると zankyo が Runtime API 本体に
//! 見える。`/next`・`/response`・`/error`・`/init/error` だけを解釈し、
//! それ以外のパスは一切触らず中継する（成功呼び出しの観測コストを
//! ゼロに近づけるため、ボディを読むのは失敗判定が必要な経路だけ）。

use crate::error::{Result, ZankyoError};
use crate::inflight::{InFlight, Invocation};
use crate::record::{
    error_context_from_body, init_request_id, response_error_context, FailureType,
};
use crate::store::Recorder;
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, HOST};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
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

pub struct ProxyState {
    /// 本来の Runtime API（`AWS_LAMBDA_RUNTIME_API` 原本の host:port）。
    pub upstream: String,
    pub client: HttpClient,
    pub inflight: Arc<InFlight>,
    pub recorder: Arc<Recorder>,
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
    let body_bytes = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
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
            match forward(
                st,
                &parts.method,
                &path_and_query,
                &parts.headers,
                body_bytes,
            )
            .await
            {
                Ok(resp) => resp.map(|b| b.boxed()),
                Err(e) => {
                    warn!(error = %e, path = %path_and_query, "upstream forward failed");
                    plain(StatusCode::BAD_GATEWAY, "zankyo: upstream unreachable")
                }
            }
        }
    }
}

/// `/next`: 応答ヘッダから requestId 等を取り、ボディ（イベント）を
/// in-flight に保持してからランタイムへ返す。
async fn handle_next(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response<BoxedBody> {
    let resp = match forward(st, method, pq, headers, body).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "invocation/next forward failed");
            return plain(StatusCode::BAD_GATEWAY, "zankyo: upstream unreachable");
        }
    };
    let (parts, resp_body) = resp.into_parts();
    let bytes = match resp_body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
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
    let resp = match forward(st, method, pq, headers, body.clone()).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, rid, "completion forward failed");
            return plain(StatusCode::BAD_GATEWAY, "zankyo: upstream unreachable");
        }
    };
    // in-flight から外すのは成否に関わらず（再配達されれば別 requestId で来る）
    let inv = st.inflight.remove(rid);
    if let Some(ctx) = ctx {
        let (recorder, inv_event, invoked_at, request_id) = match inv {
            Some(i) => (
                st.recorder.clone(),
                Some(i.event),
                i.invoked_at,
                i.request_id,
            ),
            None => (
                st.recorder.clone(),
                None,
                OffsetDateTime::now_utc(),
                rid.to_string(),
            ),
        };
        let response_payload = serde_json::from_slice::<Value>(&body).ok();
        // ランタイムへ応答を返したあと非同期で記録する（呼び出し経路を遅らせない）
        tokio::spawn(async move {
            recorder
                .save(
                    &request_id,
                    invoked_at,
                    FailureType::HandlerError,
                    inv_event,
                    response_payload,
                    ctx,
                )
                .await;
        });
    }
    resp.map(|b| b.boxed())
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
    let resp = match forward(st, method, pq, headers, body.clone()).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "init/error forward failed");
            return plain(StatusCode::BAD_GATEWAY, "zankyo: upstream unreachable");
        }
    };
    let now = OffsetDateTime::now_utc();
    let recorder = st.recorder.clone();
    let request_id = init_request_id(&now);
    tokio::spawn(async move {
        recorder
            .save(&request_id, now, FailureType::InitError, None, None, ctx)
            .await;
    });
    resp.map(|b| b.boxed())
}

/// 本来の Runtime API へ転送する。host/content-length はこちらの値に張り替える。
async fn forward(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Incoming>> {
    let uri: Uri = format!("http://{}{}", st.upstream, pq)
        .parse()
        .map_err(|e| ZankyoError::Config(format!("bad upstream uri: {e}")))?;
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(h) = builder.headers_mut() {
        for (k, v) in headers.iter() {
            if k == HOST || k == CONTENT_LENGTH {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
        if let Ok(v) = body.len().to_string().parse() {
            h.insert(CONTENT_LENGTH, v);
        }
    }
    let req = builder.body(boxed_full(body))?;
    st.client
        .request(req)
        .await
        .map_err(|e| ZankyoError::Upstream(e.to_string()))
}

fn plain(status: StatusCode, msg: &'static str) -> Response<BoxedBody> {
    Response::builder()
        .status(status)
        .body(boxed_full(Bytes::from(msg)))
        .unwrap_or_else(|_| Response::new(boxed_full(Bytes::new())))
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
