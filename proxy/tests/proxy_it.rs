//! proxy → upstream Runtime API → S3 の統合テスト。
//! 本物の HTTP サーバを立て、AWS 側だけをモックに置き換えて
//! 「イベント捕捉 → 失敗検知 → scrub → S3 Put」の経路を検証する。
//! AWS に触れないため CI で決定論的に実行できる。

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use zankyo::config::Config;
use zankyo::inflight::{InFlight, Invocation};
use zankyo::proxy::{boxed_full, new_client, serve, BoxedBody, ProxyState};
use zankyo::store::Recorder;

/// キャプチャしたリクエスト。(method, path, body)
type Captured = Arc<Mutex<Vec<(String, String, Bytes)>>>;

type HandlerFn = Arc<dyn Fn(&str, &str, &HeaderMap, &Bytes) -> Response<BoxedBody> + Send + Sync>;

/// リクエストを記録して `handler` の応答を返すモック HTTP サーバ。
async fn spawn_mock<F>(handler: F) -> (String, Captured)
where
    F: Fn(&str, &str, &HeaderMap, &Bytes) -> Response<BoxedBody> + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let handler: HandlerFn = Arc::new(handler);
    tokio::spawn({
        let captured = captured.clone();
        async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let captured = captured.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        let captured = captured.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let bytes = body
                                .collect()
                                .await
                                .map(|c| c.to_bytes())
                                .unwrap_or_default();
                            captured.lock().unwrap().push((
                                parts.method.to_string(),
                                parts.uri.path().to_string(),
                                bytes.clone(),
                            ));
                            let resp = handler(
                                parts.method.as_str(),
                                parts.uri.path(),
                                &parts.headers,
                                &bytes,
                            );
                            Ok::<_, Infallible>(resp)
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        }
    });
    (addr, captured)
}

fn captured(c: &Captured) -> Vec<(String, String, Bytes)> {
    c.lock().unwrap().clone()
}

/// 条件成立まで最大 `ms` ミリ秒待つ。間に合わなければ panic ではなく false。
async fn wait_for(ms: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}

static NEXT_SPILL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `endpoint` (mock S3) に向けた Recorder。spill はテストごとに
/// 一意の一時 dir を使い、テスト間・リラン間で残滓を共有しない。
fn recorder_to(endpoint: &str) -> Arc<Recorder> {
    let n = NEXT_SPILL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("zankyo-it-{}-{}", std::process::id(), n));
    recorder_to_with_spill(endpoint, &dir)
}

fn recorder_to_with_spill(endpoint: &str, spill: &std::path::Path) -> Arc<Recorder> {
    let conf = aws_sdk_s3::Config::builder()
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
            aws_sdk_s3::config::Credentials::for_tests(),
        ))
        .endpoint_url(format!("http://{endpoint}"))
        .force_path_style(true)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();
    let cfg = Config::from_env_map(&HashMap::from([
        ("ZANKYO_BUCKET".to_string(), "test-bucket".to_string()),
        (
            "ZANKYO_SPILL_DIR".to_string(),
            spill.to_string_lossy().into_owned(),
        ),
    ]))
    .unwrap();
    Arc::new(Recorder::new(
        aws_sdk_s3::Client::from_conf(conf),
        cfg,
        "test-fn".to_string(),
        "42".to_string(),
    ))
}

/// zankyo proxy を addr から listen させる。
async fn spawn_proxy(upstream: &str, inflight: Arc<InFlight>, recorder: Arc<Recorder>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let state = Arc::new(ProxyState {
        upstream: upstream.to_string(),
        client: new_client(),
        inflight,
        cfg: recorder.config().clone(),
        recorder,
        pending: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
    });
    tokio::spawn(serve(listener, state));
    addr
}

fn ok_body(v: Value) -> Response<BoxedBody> {
    Response::builder()
        .status(StatusCode::OK)
        .body(boxed_full(Bytes::from(v.to_string())))
        .unwrap()
}

fn ok_empty() -> Response<BoxedBody> {
    Response::builder()
        .status(StatusCode::OK)
        .body(boxed_full(Bytes::new()))
        .unwrap()
}

/// `/next` に PII 混じりのイベント、他パスは 200 を返す Runtime API モック。
fn runtime_api_handler() -> impl Fn(&str, &str, &HeaderMap, &Bytes) -> Response<BoxedBody> {
    move |method, path, _headers, _body| {
        if method == "GET" && path == "/2018-06-01/runtime/invocation/next" {
            return Response::builder()
                .status(StatusCode::OK)
                .header("lambda-runtime-aws-request-id", "req-123")
                .header("lambda-runtime-deadline-ms", "999999")
                .body(boxed_full(Bytes::from(
                    json!({
                        "email": "alice@example.com",
                        "password": "hunter2",
                        "data": {"keep": 1}
                    })
                    .to_string(),
                )))
                .unwrap();
        }
        ok_empty()
    }
}

async fn call(proxy: &str, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Bytes) {
    call_raw(
        proxy,
        method,
        path,
        match body {
            Some(v) => Bytes::from(v.to_string()),
            None => Bytes::new(),
        },
    )
    .await
}

async fn call_raw(proxy: &str, method: Method, path: &str, body: Bytes) -> (StatusCode, Bytes) {
    let client = new_client();
    let req = Request::builder()
        .method(method)
        .uri(format!("http://{proxy}{path}"))
        .body(boxed_full(body))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    (parts.status, body.collect().await.unwrap().to_bytes())
}

#[tokio::test]
async fn handler_error_is_forwarded_scrubbed_and_recorded() {
    let (api_addr, api_hits) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| {
        Response::builder()
            .status(StatusCode::OK)
            .header("ETag", "\"mock\"")
            .body(boxed_full(Bytes::new()))
            .unwrap()
    })
    .await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder_to(&s3_addr)).await;

    // 1. ランタイムが /next をポーリング → イベントは素通り（中身を変えない）
    let (status, body) = call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("alice@example.com"));
    assert_eq!(inflight.len(), 1);

    // 2. ランタイムが /error を呼ぶ → 上流へ転送 + 記録パイプラインへ
    let (status, _) = call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/error",
        Some(json!({"errorType": "Error", "errorMessage": "boom"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        wait_for(2_000, || captured(&api_hits)
            .iter()
            .any(|(_, p, _)| p.contains("/invocation/req-123/error")))
        .await
    );

    // 3. S3 にレコードが届く: イベントは scrub 済みでエラー文脈付き
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (method, path, body) = captured(&s3_hits)
        .into_iter()
        .find(|(m, p, _)| m == "PUT" && p.contains("req-123.json"))
        .expect("PUT zankyo record");
    assert_eq!(method, "PUT");
    assert!(path.contains("/test-bucket/zankyo/test-fn/"));
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["version"], "1");
    assert_eq!(rec["functionName"], "test-fn");
    assert_eq!(rec["failureType"], "handler_error");
    assert_eq!(rec["requestId"], "req-123");
    // scrub: フィールド名とパターン両方が効いていること
    assert_eq!(rec["event"]["password"], "h***");
    assert_eq!(rec["event"]["email"], "a***@e***.com");
    assert_eq!(rec["event"]["data"]["keep"], 1);
    assert_eq!(rec["errorContext"]["errorType"], "Error");
    assert!(rec["scrubReport"]["fieldsRedacted"].as_u64().unwrap() >= 1);
    assert_eq!(inflight.len(), 0);
}

#[tokio::test]
async fn successful_response_is_not_recorded() {
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder_to(&s3_addr)).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    let (status, _) = call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/response",
        Some(json!({"ok": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inflight.len(), 0);
    // 成功応答は記録しない: 少し待っても S3 に PUT が来ないこと
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(captured(&s3_hits).is_empty());
}

#[tokio::test]
async fn response_with_error_type_is_recorded() {
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder_to(&s3_addr)).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    // /response 経路でも errorType 含有は失敗として扱う
    let (status, _) = call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/response",
        Some(json!({"errorType": "Runtime.UserError", "errorMessage": "handled"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, _, body) = captured(&s3_hits).remove(0);
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["failureType"], "handler_error");
    assert_eq!(rec["errorContext"]["errorType"], "Runtime.UserError");
}

#[tokio::test]
async fn init_error_records_without_event() {
    let (api_addr, _api) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight, recorder_to(&s3_addr)).await;

    let (status, _) = call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/init/error",
        Some(json!({"errorType": "Runtime.ExitError", "errorMessage": "init boom"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, path, body) = captured(&s3_hits).remove(0);
    assert!(path.contains("init-"));
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["failureType"], "init_error");
    assert_eq!(rec["event"], Value::Null);
    assert_eq!(rec["errorContext"]["errorType"], "Runtime.ExitError");
}

#[tokio::test]
async fn shutdown_flushes_inflight_as_timeout() {
    // Extensions API モック: /event/next は常に SHUTDOWN を返す
    let (api_addr, _api) = spawn_mock(|_m, path, _h, _b| {
        if path.contains("/extension/event/next") {
            return ok_body(json!({
                "eventType": "SHUTDOWN",
                "shutdownReason": "timeout",
                "deadlineMs": OffsetDateTime::now_utc().unix_timestamp() * 1000 + 60_000
            }));
        }
        ok_empty()
    })
    .await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;

    let inflight = Arc::new(InFlight::new());
    inflight.insert(Invocation {
        request_id: "req-timeout".to_string(),
        event: json!({"token": "secret-token-value", "input": 7}),
        invoked_at: OffsetDateTime::now_utc(),
    });
    let spill_dir = std::env::temp_dir().join(format!("zankyo-it-shutdown-{}", std::process::id()));
    let recorder = recorder_to_with_spill(&s3_addr, &spill_dir);

    zankyo::extension::run_event_loop(
        new_client(),
        api_addr,
        "ext-id".to_string(),
        inflight.clone(),
        recorder,
    )
    .await;

    assert_eq!(inflight.len(), 0);
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, path, body) = captured(&s3_hits).remove(0);
    assert!(path.contains("req-timeout.json"));
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["failureType"], "timeout");
    assert_eq!(rec["errorContext"]["errorType"], "Timeout");
    // timeout 経路でも scrub は効く
    assert_eq!(rec["event"]["token"], "s***");
    // S3 へ届いた spill は成功時に掃除される（残すと次回 init で冗長 PUT）
    let spill = spill_dir.join("req-timeout.json");
    assert!(!spill.exists());
    let _ = std::fs::remove_dir_all(&spill_dir);
}

#[tokio::test]
async fn non_runtime_paths_are_forwarded_without_recording() {
    // 対象外パス（telemetry API 等）はボディごと中継するだけで記録しない
    let (api_addr, api_hits) = spawn_mock(|_m, _p, _h, _b| ok_body(json!({"proxied": true}))).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight, recorder_to(&s3_addr)).await;

    let (status, body) = call(
        &proxy,
        Method::POST,
        "/2022-07-01/telemetry",
        Some(json!({"types": ["platform.logs"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("proxied"));
    assert!(
        wait_for(1_000, || {
            captured(&api_hits)
                .iter()
                .any(|(_, p, _)| p == "/2022-07-01/telemetry")
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(captured(&s3_hits).is_empty());
}

#[tokio::test]
async fn oversized_body_is_rejected_without_forwarding() {
    let (api_addr, api_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (s3_addr, _s3) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight, recorder_to(&s3_addr)).await;

    // MAX_BODY_BYTES (8MiB) を超えるボディは上流へ転送せず 413 を返す
    let big = Bytes::from(vec![b'x'; 9 * 1024 * 1024]);
    let (status, _) = call_raw(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-x/response",
        big,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(captured(&api_hits).is_empty());
}
