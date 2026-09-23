//! Runtime API プロキシ。
//!
//! 子プロセス（実ランタイム）から見ると zankyo が Runtime API 本体に
//! 見える。`/next`・`/response`・`/error`・`/init/error` だけを解釈し、
//! それ以外のパスは一切触らず中継する（成功呼び出しの観測コストを
//! ゼロに近づけるため、ボディを読むのは失敗判定が必要な経路だけ）。
//! 各ルートの処理は `handlers.rs`、上流転送は `upstream.rs`。

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
use std::convert::Infallible;
use std::sync::Arc;
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

/// バックグラウンド回収タスク（init 時の inflight 変換＋spill 再送）の
/// JoinSet。終了時に drain して途中の PUT を打ち切らないための管理。
/// 失敗レコードの保存自体はハンドラ内でインライン実行される
/// （転送前保存）ため、ここに積むのは起動経路の回収処理だけ。
pub struct Pending {
    pub set: tokio::task::JoinSet<()>,
}

impl Pending {
    pub fn new() -> Self {
        Self {
            set: tokio::task::JoinSet::new(),
        }
    }
}

impl Default for Pending {
    fn default() -> Self {
        Self::new()
    }
}

pub type PendingSaves = tokio::sync::Mutex<Pending>;

pub struct ProxyState {
    /// 本来の Runtime API（`AWS_LAMBDA_RUNTIME_API` 原本の host:port）。
    pub upstream: String,
    pub client: HttpClient,
    pub inflight: Arc<crate::inflight::InFlight>,
    pub recorder: Arc<Recorder>,
    /// ボディ上限などの動作ノブ。env / SSM 由来の値をそのまま使う。
    pub cfg: crate::config::Config,
    /// 進行中の save タスク。子終了時に残っていれば drain される。
    pub pending: PendingSaves,
    /// 処理中のハンドラ数。drain が「これ以上 save が増えない」
    /// 地点を判断するのに使う。
    pub active: std::sync::atomic::AtomicUsize,
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
    st.active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _active = ActiveGuard { count: &st.active };
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
    tracing::info!(method = %parts.method, path = %path, "runtime api request");
    match (parts.method.as_str(), segs.as_slice()) {
        ("GET", ["", "2018-06-01", "runtime", "invocation", "next"]) => {
            crate::handlers::handle_next(
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
            crate::handlers::handle_completion(
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
            crate::handlers::handle_init_error(
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
pub(crate) async fn forward_or_502(
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

/// handle() の在席数を戻す RAII ガード。
struct ActiveGuard<'a> {
    count: &'a std::sync::atomic::AtomicUsize,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 上流の応答をランタイムへ返す形へ整える。hop-by-hop ヘッダを落とし、
/// ボディをバッファ済みに揃える。
pub(crate) fn boxed_response(mut resp: Response<Incoming>) -> Response<BoxedBody> {
    strip_hop_by_hop(resp.headers_mut());
    resp.map(|b| b.boxed())
}

/// drain 中に新たな save が増えないか確認する待機間隔。
const DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// 積まれた回収タスクが空になるまで待つ。全体で `budget` まで —
/// S3 が応答しない環境で子プロセスの終了を無制限に遅らせない。
/// その後 `active` が 0 になるまで待ち、転送前保存を実行中の
/// ハンドラも拾い切る。
pub(crate) async fn drain_pending(state: &ProxyState, budget: std::time::Duration) {
    let drain = async {
        {
            let mut p = state.pending.lock().await;
            while let Some(res) = p.set.join_next().await {
                // panic した回収タスクはレコードを失う — 数えて警告に残す
                if let Err(e) = res {
                    warn!(error = %e, "pending record save panicked");
                }
            }
        }
        // ハンドラ内のインライン save（転送前保存）は JoinSet に
        // 載らないため、ハンドラが居なくなるまで待つ。
        while state.active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            tokio::time::sleep(DRAIN_POLL).await;
        }
    };
    if tokio::time::timeout(budget, drain).await.is_err() {
        warn!("pending record saves did not finish before exit");
    }
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
