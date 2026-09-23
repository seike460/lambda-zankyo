//! 本来の Runtime API への転送とボディ読み取り。
//! proxy.rs（経路判定・記録判定）から上流通信の詳細を分離する。

use crate::error::{Result, ZankyoError};
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, HOST};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::body::Incoming;

use crate::proxy::{boxed_full, BoxedBody, ProxyState};

/// RFC 9110 §7.6.1 の hop-by-hop ヘッダ。プロキシがそのまま転送すると
/// 上流の接続管理を壊すため必ず落とす。
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 本来の Runtime API へ転送する。host/content-length はこちらの値に
/// 張り替え、hop-by-hop ヘッダ（connection 等）は上流へ渡さない。
/// ボディは collect 済みなので chunked 関連ヘッダは意味を失う。
/// `timeout` が Some のとき、その時間で打ち切る。
pub async fn forward(
    st: &ProxyState,
    method: &Method,
    pq: &str,
    headers: &HeaderMap,
    body: Bytes,
    timeout: Option<std::time::Duration>,
) -> Result<Response<Incoming>> {
    let uri: Uri = format!("http://{}{}", st.upstream, pq)
        .parse()
        .map_err(|e| ZankyoError::Config(format!("bad upstream uri: {e}")))?;
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(h) = builder.headers_mut() {
        // RFC 9110: Connection ヘッダは「この接続でだけ有効な追加ヘッダ」を
        // 指名できる。固定の hop-by-hop 一覧だけでなく、指名された
        // ヘッダも上流へ渡さない。
        let drop_named = connection_named(headers);
        for (k, v) in headers.iter() {
            if k == HOST
                || k == CONTENT_LENGTH
                || HOP_BY_HOP.contains(&k.as_str())
                || drop_named.iter().any(|n| n == k.as_str())
            {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
        if let Ok(v) = body.len().to_string().parse() {
            h.insert(CONTENT_LENGTH, v);
        }
    }
    let req = builder.body(boxed_full(body))?;
    let call = st.client.request(req);
    match timeout {
        Some(t) => tokio::time::timeout(t, call)
            .await
            .map_err(|_| ZankyoError::Upstream("upstream forward timed out".into()))?
            .map_err(|e| ZankyoError::Upstream(e.to_string())),
        None => call.await.map_err(|e| ZankyoError::Upstream(e.to_string())),
    }
}

/// `Connection` ヘッダが指名する追加の hop-by-hop ヘッダ名を集める。
fn connection_named(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .collect()
}

/// 上流→ランタイム方向の応答ヘッダにも同じ規則を適用する。
/// 上流が `transfer-encoding` や Connection 指名ヘッダを返した場合、
/// 素通しするとランタイム側のフレーミングと食い違うため除去する。
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let drop_named = connection_named(headers);
    let drop: Vec<http::HeaderName> = headers
        .keys()
        .filter(|k| HOP_BY_HOP.contains(&k.as_str()) || drop_named.iter().any(|n| n == k.as_str()))
        .cloned()
        .collect();
    for k in drop {
        headers.remove(k);
    }
}

pub enum CollectError {
    TooLarge,
    Read(Box<dyn std::error::Error + Send + Sync>),
}

/// `limit` バイトまでボディを読む。`http_body_util::Limited` は超過時に
/// `LengthLimitError` を返すので、通常の読み取り失敗と分けて扱う。
pub async fn collect_bounded(
    body: Incoming,
    limit: usize,
) -> std::result::Result<Bytes, CollectError> {
    http_body_util::Limited::new(body, limit)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                CollectError::TooLarge
            } else {
                CollectError::Read(e)
            }
        })
}

/// プレーンテキスト応答の組み立て。
pub fn plain(status: StatusCode, msg: &'static str) -> Response<BoxedBody> {
    Response::builder()
        .status(status)
        .body(boxed_full(Bytes::from(msg)))
        .unwrap_or_else(|_| Response::new(boxed_full(Bytes::new())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn hop_by_hop_headers_cover_rfc9110() {
        // 転送してはいけない代表的ヘッダが全部含まれていること
        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(HOP_BY_HOP.contains(&name), "{name} must be dropped");
        }
    }

    #[test]
    fn header_name_matching_is_case_insensitive() {
        // http::HeaderName は常に小文字正規化されるので as_str() 比較で効く
        let mut h = HeaderMap::new();
        h.insert("Connection", HeaderValue::from_static("keep-alive"));
        let name = h.keys().next().unwrap().as_str();
        assert_eq!(name, "connection");
        assert!(HOP_BY_HOP.contains(&name));
    }

    #[test]
    fn connection_header_names_are_parsed() {
        // "Connection: x-opt, close" は x-opt も drop 対象にする
        let mut h = HeaderMap::new();
        h.insert(
            "connection",
            HeaderValue::from_static("X-Custom-Hop, keep-alive"),
        );
        let mut drop_named: Vec<String> = Vec::new();
        for v in h.get_all("connection") {
            if let Ok(s) = v.to_str() {
                drop_named.extend(s.split(',').map(|t| t.trim().to_ascii_lowercase()));
            }
        }
        assert!(drop_named.iter().any(|n| n == "x-custom-hop"));
        assert!(drop_named.iter().any(|n| n == "keep-alive"));
    }
}
