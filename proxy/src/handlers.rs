//! Runtime API の個別ルートハンドラ。
//!
//! ルーティング・接続管理は `proxy.rs`、上流への転送は `upstream.rs`。
//! ここでは各エンドポイント固有の判定（イベント捕捉・失敗記録の
//! 起動）だけを扱う。

use crate::inflight::{EventEncoding, Invocation};
use crate::proxy::{
    boxed_full, boxed_response, forward_or_502, BoxedBody, PendingSaves, ProxyState,
};
use crate::record::{
    error_context_from_body, init_request_id, response_error_context, FailureType,
};
use crate::store::{EventInput, Recorder, SaveInput};
use crate::upstream::{collect_bounded, plain, strip_hop_by_hop, CollectError};
use base64::Engine;
use bytes::Bytes;
use http::{HeaderMap, Method, Response, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use time::OffsetDateTime;
use tracing::warn;

/// ランタイムへの応答を遅らせないよう、レコード保存は非同期で行う。
/// タスクは `pending` に積まれ、プロセス終了前に orchestrate が
/// ドレインする（投げっぱなしにすると init_error 等の記録が消える）。
/// 終了処理で pending が closed 済みなら JoinSet へ積んでも
/// 誰も await しないため、その場合は呼び出し側で同期的に保存する。
async fn spawn_save(pending: &PendingSaves, recorder: Arc<Recorder>, job: SaveInput) {
    let mut p = pending.lock().await;
    if p.closed {
        drop(p);
        recorder.save(job).await;
        return;
    }
    p.set.spawn(async move {
        recorder.save(job).await;
    });
}

/// `/next`: 応答ヘッダから requestId 等を取り、ボディ（イベント）を
/// in-flight に保持してからランタイムへ返す。
pub(crate) async fn handle_next(
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
            // JSON でないイベントは UTF-8 なら生テキスト、それ以外は
            // base64 で保持する。from_utf8_lossy は U+FFFD に潰れて
            // replay で元イベントと異なる入力になるため使わない。
            let (event, encoding) = match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => (v, EventEncoding::Json),
                Err(_) => match String::from_utf8(bytes.to_vec()) {
                    Ok(s) => (Value::String(s), EventEncoding::RawText),
                    Err(_) => (
                        Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                        EventEncoding::Base64,
                    ),
                },
            };
            st.inflight.insert(Invocation {
                request_id,
                event,
                encoding,
                invoked_at: OffsetDateTime::now_utc(),
            });
        }
    }
    Response::from_parts(parts, boxed_full(bytes))
}

/// `/response`（成功扱いだが errorType 含有は失敗）と `/error`（失敗）。
pub(crate) async fn handle_completion(
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
    // 失敗経路では remove と記録権の確保を原子的に行う — forward 中に
    // 到着した 2 回目の /error が event 欠落のまま先に記録権を取り、
    // イベント保持側の記録を捨てさせる競合を防ぐ。
    let (inv, claimed) = if ctx.is_some() {
        st.inflight.remove_and_claim(rid)
    } else {
        (st.inflight.remove(rid), false)
    };
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
        // 同一 requestId の失敗記録は 1 件。上流断で 502 を返した後の
        // ランタイム再試行や、SHUTDOWN drain との競合で event 欠落・
        // errorContext 欠落の記録が同一キーを上書きするのを防ぐ。
        if claimed {
            let (event, encoding, invoked_at, request_id) = match inv {
                Some(i) => (Some(i.event), i.encoding, i.invoked_at, i.request_id),
                None => (
                    None,
                    EventEncoding::Json,
                    OffsetDateTime::now_utc(),
                    rid.to_string(),
                ),
            };
            spawn_save(
                &st.pending,
                st.recorder.clone(),
                SaveInput {
                    request_id,
                    invoked_at,
                    failure: FailureType::HandlerError,
                    event: EventInput {
                        value: event,
                        encoding,
                    },
                    response: serde_json::from_slice::<Value>(&body).ok(),
                    ctx,
                },
            )
            .await;
        }
    }
    resp.map(boxed_response).unwrap_or_else(|r| *r)
}

/// `/init/error`: 対応するイベントが存在しないため event は null。
pub(crate) async fn handle_init_error(
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
    let rid = init_request_id(&now);
    // 擬似 requestId は nanos 粒度だが、万一同じキーが来ても
    // dedupe 集合で二重記録を防ぐ。
    if st.inflight.claim_record(&rid) {
        spawn_save(
            &st.pending,
            st.recorder.clone(),
            SaveInput {
                request_id: rid,
                invoked_at: now,
                failure: FailureType::InitError,
                event: EventInput {
                    value: None,
                    encoding: EventEncoding::Json,
                },
                response: None,
                ctx,
            },
        )
        .await;
    }
    resp.map(boxed_response).unwrap_or_else(|r| *r)
}
