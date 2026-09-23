//! 本来の Runtime API への転送とボディ読み取り。
//! proxy.rs（経路判定・記録判定）から上流通信の詳細を分離する。

use crate::error::{Result, ZankyoError};
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, HOST};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::body::Incoming;

use crate::proxy::{boxed_full, BoxedBody, ProxyState};

/// 中継するボディの上限。Lambda の同期呼び出しペイロード上限（6MiB）に
/// 余裕を持たせた値で、異常な巨大ボディによるメモリ圧迫を防ぐ。
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

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
pub async fn forward(
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
            if k == HOST || k == CONTENT_LENGTH || HOP_BY_HOP.contains(&k.as_str()) {
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

pub enum CollectError {
    TooLarge,
    Read(Box<dyn std::error::Error + Send + Sync>),
}

/// 上限付きでボディを読む。`http_body_util::Limited` は超過時に
/// `LengthLimitError` を返すので、通常の読み取り失敗と分けて扱う。
pub async fn collect_bounded(body: Incoming) -> std::result::Result<Bytes, CollectError> {
    http_body_util::Limited::new(body, MAX_BODY_BYTES)
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
}
