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
use zankyo::inflight::{EventEncoding, InFlight, Invocation};
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

/// テストごとに一意な spill dir。Drop で消すので、アサーションの
/// 失敗で抜けた場合も一時ディレクトリが残らない。
struct SpillDir(std::path::PathBuf);

impl SpillDir {
    fn new() -> Self {
        let n = NEXT_SPILL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("zankyo-it-{}-{}", std::process::id(), n)))
    }
}

impl std::ops::Deref for SpillDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `endpoint` (mock S3) に向けた Recorder と、その spill dir。
/// spill dir は戻り値を保持している間だけ残る。
fn recorder_to(endpoint: &str) -> (Arc<Recorder>, SpillDir) {
    recorder_with_env(endpoint, &[])
}

/// `recorder_to` に env 設定を足したもの。
fn recorder_with_env(endpoint: &str, extra: &[(&str, &str)]) -> (Arc<Recorder>, SpillDir) {
    let spill = SpillDir::new();
    let conf = aws_sdk_s3::Config::builder()
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
            aws_sdk_s3::config::Credentials::for_tests(),
        ))
        .endpoint_url(format!("http://{endpoint}"))
        .force_path_style(true)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();
    let mut env = HashMap::from([
        ("ZANKYO_BUCKET".to_string(), "test-bucket".to_string()),
        (
            "ZANKYO_SPILL_DIR".to_string(),
            spill.to_string_lossy().into_owned(),
        ),
    ]);
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let cfg = Config::from_env_map(&env).unwrap();
    let recorder = Arc::new(Recorder::new(
        aws_sdk_s3::Client::from_conf(conf),
        cfg,
        "test-fn".to_string(),
        "42".to_string(),
    ));
    (recorder, spill)
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
        pending: tokio::sync::Mutex::new(zankyo::proxy::Pending::new()),
        active: std::sync::atomic::AtomicUsize::new(0),
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
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder).await;

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
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder).await;

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
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight.clone(), recorder).await;

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
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

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
        encoding: EventEncoding::Json,
        invoked_at: OffsetDateTime::now_utc(),
    });
    let (recorder, spill_dir) = recorder_to(&s3_addr);

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
    let spill = spill_dir.join("zankyo-req-timeout.json");
    assert!(!spill.exists());
}

#[tokio::test]
async fn non_runtime_paths_are_forwarded_without_recording() {
    // 対象外パス（telemetry API 等）はボディごと中継するだけで記録しない
    let (api_addr, api_hits) = spawn_mock(|_m, _p, _h, _b| ok_body(json!({"proxied": true}))).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

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
async fn raw_text_event_is_recorded_verbatim() {
    // 非 JSON だが UTF-8 のイベントは生テキストで保持する
    let (api_addr, _api) = spawn_mock(|method, path, _h, _b| {
        if method == "GET" && path == "/2018-06-01/runtime/invocation/next" {
            return Response::builder()
                .status(StatusCode::OK)
                .header("lambda-runtime-aws-request-id", "req-raw")
                .body(boxed_full(Bytes::from("<xml>not json</xml>")))
                .unwrap();
        }
        ok_empty()
    })
    .await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-raw/error",
        Some(json!({"errorType": "E", "errorMessage": "x"})),
    )
    .await;
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, _, body) = captured(&s3_hits).remove(0);
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["eventIsRawText"], true);
    assert_eq!(rec["event"], "<xml>not json</xml>");
}

#[tokio::test]
async fn binary_event_is_stored_as_base64() {
    // UTF-8 でないイベントは base64 で保持し、デコードで元バイト列が復元できる
    let original: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe];
    let (api_addr, _api) = spawn_mock(move |method, path, _h, _b| {
        if method == "GET" && path == "/2018-06-01/runtime/invocation/next" {
            return Response::builder()
                .status(StatusCode::OK)
                .header("lambda-runtime-aws-request-id", "req-bin")
                .body(boxed_full(Bytes::from_static(original)))
                .unwrap();
        }
        ok_empty()
    })
    .await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-bin/error",
        Some(json!({"errorType": "E", "errorMessage": "x"})),
    )
    .await;
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, _, body) = captured(&s3_hits).remove(0);
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["eventIsBase64"], true);
    let stored = rec["event"].as_str().expect("base64 event");
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .unwrap();
    assert_eq!(decoded, original);
}

#[tokio::test]
async fn error_context_pii_is_scrubbed() {
    // errorMessage に含まれる PII も scrubReport 集約でマスクされる
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/error",
        Some(json!({
            "errorType": "E",
            "errorMessage": "duplicate email alice@example.com"
        })),
    )
    .await;
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, _, body) = captured(&s3_hits).remove(0);
    let rec: Value = serde_json::from_slice(&body).unwrap();
    let msg = rec["errorContext"]["errorMessage"].as_str().unwrap();
    assert!(
        !msg.contains("alice@example.com"),
        "errorMessage should be scrubbed: {msg}"
    );
    assert!(msg.contains("***"));
}

#[tokio::test]
async fn scrub_off_keeps_error_context_verbatim() {
    // ZANKYO_SCRUB_MODE=off は errorMessage・stackTrace の自由テキストも書き換えない
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_with_env(&s3_addr, &[("ZANKYO_SCRUB_MODE", "off")]);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    let message = "duplicate email alice@example.com key AKIAIOSFODNN7EXAMPLE";
    let trace = [
        "Error: Bearer abcdefghijk123",
        "    at handler (index.js:3:9)",
    ];
    call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/error",
        Some(json!({"errorType": "E", "errorMessage": message, "stackTrace": trace})),
    )
    .await;
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, _, body) = captured(&s3_hits).remove(0);
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["event"]["password"], "hunter2");
    assert_eq!(rec["errorContext"]["errorMessage"], message);
    assert_eq!(rec["errorContext"]["stackTrace"], trace.join("\n"));
    assert_eq!(rec["scrubReport"]["patternsApplied"], json!([]));
}

#[tokio::test]
async fn second_error_call_does_not_overwrite_record() {
    // 同一 requestId の /error 再試行は記録済みフラグで抑止される
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    for _ in 0..2 {
        call(
            &proxy,
            Method::POST,
            "/2018-06-01/runtime/invocation/req-123/error",
            Some(json!({"errorType": "E", "errorMessage": "x"})),
        )
        .await;
    }
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let puts = captured(&s3_hits)
        .into_iter()
        .filter(|(m, p, _)| m == "PUT" && p.contains("req-123.json"))
        .count();
    assert_eq!(puts, 1);
}

#[tokio::test]
async fn inflight_stage_is_cleared_on_completion() {
    // /next で .inflight ステージが書かれ、完了（成功・失敗とも）で消える。
    // 残ったステージは「応答なく畳まれた呼び出し」の証跡なので、
    // 正常完了分が残ると次回 init で誤って timeout 記録される。
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let (s3_addr, _s3) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    let inflight = Arc::new(InFlight::new());
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;
    let stage = spill_dir.join("zankyo-req-123.inflight");

    call(
        &proxy,
        Method::GET,
        "/2018-06-01/runtime/invocation/next",
        None,
    )
    .await;
    assert!(stage.exists());
    let (status, _) = call(
        &proxy,
        Method::POST,
        "/2018-06-01/runtime/invocation/req-123/response",
        Some(json!({"ok": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!stage.exists());
}

#[tokio::test]
async fn leftover_inflight_stage_becomes_timeout_record() {
    // external extension / init 時回収: 残った .inflight ステージを
    // timeout レコードへ変換し、ステージ本体は消える。
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    let inv = Invocation {
        request_id: "req-stuck".to_string(),
        event: json!({"token": "secret-token-value"}),
        encoding: EventEncoding::Json,
        invoked_at: OffsetDateTime::now_utc(),
    };
    recorder.stage_inflight(&inv);
    let stage = spill_dir.join("zankyo-req-stuck.inflight");
    assert!(stage.exists());

    recorder.recover_inflights(Some("timeout")).await;

    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (_, path, body) = captured(&s3_hits).remove(0);
    assert!(path.contains("req-stuck.json"));
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["failureType"], "timeout");
    assert_eq!(rec["event"]["token"], "s***");
    // SHUTDOWN reason は errorContext へ引き継ぐ
    assert_eq!(rec["errorContext"]["errorType"], "Timeout");
    // 変換後はステージも spill も残らない（PUT 成功時）
    assert!(!stage.exists());
    assert!(!spill_dir.join("zankyo-req-stuck.json").exists());
}

/// `up` が false の間は PutObject を 403 で拒否する S3 モック。
/// 403 は SDK が再試行しないため、失敗が 1 回の PUT で確定する。
async fn spawn_switchable_s3() -> (String, Captured, Arc<std::sync::atomic::AtomicBool>) {
    let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (addr, hits) = spawn_mock({
        let up = up.clone();
        move |_m, _p, _h, _b| {
            if up.load(std::sync::atomic::Ordering::SeqCst) {
                return ok_empty();
            }
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(boxed_full(Bytes::from_static(
                    b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
                )))
                .unwrap()
        }
    })
    .await;
    (addr, hits, up)
}

#[tokio::test]
async fn inflight_stage_survives_when_spill_and_put_both_fail() {
    // spill の書き込みと S3 PUT がともに失敗すると、.inflight ステージが
    // timeout レコードの最後のコピーになる。消さずに残し、両方が回復した後の
    // 回収で記録できること。
    let (s3_addr, s3_hits, s3_up) = spawn_switchable_s3().await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    recorder.stage_inflight(&Invocation {
        request_id: "req-last".to_string(),
        event: json!({"input": 1}),
        encoding: EventEncoding::Json,
        invoked_at: OffsetDateTime::now_utc(),
    });
    let stage = spill_dir.join("zankyo-req-last.inflight");
    // spill の書き込み先をディレクトリで塞ぎ、rename を失敗させる
    let spill_blocker = spill_dir.join("zankyo-req-last.json");
    std::fs::create_dir(&spill_blocker).unwrap();
    let record_puts = || {
        captured(&s3_hits)
            .into_iter()
            .filter(|(m, p, _)| m == "PUT" && p.contains("req-last.json"))
            .count()
    };

    recorder.recover_inflights(None).await;

    assert_eq!(record_puts(), 1, "the PUT must have been attempted");
    assert!(stage.exists(), "the last copy of the record must survive");

    std::fs::remove_dir(&spill_blocker).unwrap();
    s3_up.store(true, std::sync::atomic::Ordering::SeqCst);
    recorder.recover_inflights(None).await;

    assert_eq!(record_puts(), 2);
    assert!(!stage.exists());
    assert!(!spill_blocker.exists());
}

#[tokio::test]
async fn unreadable_inflight_stage_is_kept_for_retry() {
    // 読み取りの失敗は一時的でありうる。パースできないステージとは違い、
    // 最後の証跡なので消さない。読めるようになった後の回収で記録する。
    use std::os::unix::fs::PermissionsExt;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    recorder.stage_inflight(&Invocation {
        request_id: "req-locked".to_string(),
        event: json!({"input": 1}),
        encoding: EventEncoding::Json,
        invoked_at: OffsetDateTime::now_utc(),
    });
    let stage = spill_dir.join("zankyo-req-locked.inflight");
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&stage).is_ok() {
        // root はモードに関係なく読めるため、この条件を作れない
        return;
    }

    recorder.recover_inflights(None).await;

    assert!(stage.exists(), "an unreadable stage must not be deleted");
    assert!(captured(&s3_hits).is_empty());

    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o600)).unwrap();
    recorder.recover_inflights(None).await;

    assert_eq!(captured(&s3_hits).len(), 1);
    assert!(!stage.exists());
}

#[tokio::test]
async fn failed_put_keeps_spill_until_recovery_resends_it() {
    // S3 が PutObject を拒否する間は spill が残り、回復後の recover_spills が
    // 同じキーへ同じレコードを再送して spill を消す（write-ahead の本線）。
    let (api_addr, _api) = spawn_mock(runtime_api_handler()).await;
    let s3_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s3_addr, s3_hits) = spawn_mock({
        let s3_up = s3_up.clone();
        move |_m, _p, _h, _b| {
            if s3_up.load(std::sync::atomic::Ordering::SeqCst) {
                return ok_empty();
            }
            Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(boxed_full(Bytes::from_static(
                    b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
                )))
                .unwrap()
        }
    })
    .await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, Arc::new(InFlight::new()), recorder.clone()).await;
    let spilled = spill_dir.join("zankyo-req-123.json");
    let record_puts = || {
        captured(&s3_hits)
            .into_iter()
            .filter(|(m, p, _)| m == "PUT" && p.contains("req-123.json"))
            .collect::<Vec<_>>()
    };

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
        "/2018-06-01/runtime/invocation/req-123/error",
        Some(json!({"errorType": "E", "errorMessage": "x"})),
    )
    .await;
    // 記録の失敗は関数の応答を止めない
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record_puts().len(), 1);
    assert!(spilled.exists(), "rejected PUT must keep the spill");
    assert!(!spill_dir.join("zankyo-req-123.inflight").exists());

    s3_up.store(true, std::sync::atomic::Ordering::SeqCst);
    recorder.recover_spills().await;

    let puts = record_puts();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[1].1, puts[0].1, "resend must target the same key");
    assert_eq!(puts[1].2, puts[0].2, "resend must carry the same record");
    assert!(!spilled.exists());
    // 空になった spill dir は消さない。消すと、同時に走るステージや
    // spill の書き込みが ENOENT で失敗する。
    assert!(spill_dir.exists());
}

/// S3 モックが PutObject で受けた (x-amz-server-side-encryption, KMS キー ID)。
type SseHeaders = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

#[tokio::test]
async fn record_put_requests_server_side_encryption() {
    // 既定は SSE-S3（AES256）。ZANKYO_KMS_KEY 指定時は SSE-KMS とそのキー
    let kms = "arn:aws:kms:us-east-1:111122223333:key/test";
    for (extra, want) in [
        (vec![], (Some("AES256"), None)),
        (vec![("ZANKYO_KMS_KEY", kms)], (Some("aws:kms"), Some(kms))),
    ] {
        let seen: SseHeaders = Arc::new(Mutex::new(Vec::new()));
        let (s3_addr, _s3) = spawn_mock({
            let seen = seen.clone();
            move |m, _p, h, _b| {
                if m == "PUT" {
                    let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
                    seen.lock().unwrap().push((
                        header("x-amz-server-side-encryption"),
                        header("x-amz-server-side-encryption-aws-kms-key-id"),
                    ));
                }
                ok_empty()
            }
        })
        .await;
        let (recorder, _spill) = recorder_with_env(&s3_addr, &extra);
        recorder.stage_inflight(&Invocation {
            request_id: "req-sse".to_string(),
            event: json!({"input": 1}),
            encoding: EventEncoding::Json,
            invoked_at: OffsetDateTime::now_utc(),
        });
        recorder.recover_inflights(None).await;

        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![(want.0.map(String::from), want.1.map(String::from))],
            "{extra:?}"
        );
    }
}

#[tokio::test]
async fn hop_by_hop_headers_are_dropped_in_both_directions() {
    // 上流へは Connection 指名ヘッダと固定の hop-by-hop を渡さず、
    // 上流の応答も同じ規則で落としてからランタイムへ返す
    let upstream_saw = Arc::new(Mutex::new(HeaderMap::new()));
    let (api_addr, _api) = spawn_mock({
        let upstream_saw = upstream_saw.clone();
        move |_m, _p, h, _b| {
            *upstream_saw.lock().unwrap() = h.clone();
            Response::builder()
                .status(StatusCode::OK)
                .header("connection", "X-Up-Hop")
                .header("x-up-hop", "1")
                .header("x-up-keep", "1")
                .body(boxed_full(Bytes::new()))
                .unwrap()
        }
    })
    .await;
    let (s3_addr, _s3) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, Arc::new(InFlight::new()), recorder).await;

    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{proxy}/2022-07-01/telemetry"))
        .header("connection", "X-Custom-Hop")
        .header("x-custom-hop", "1")
        .header("proxy-authorization", "Basic abc")
        .header("x-keep", "1")
        .body(boxed_full(Bytes::from_static(b"{}")))
        .unwrap();
    let resp = new_client().request(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let sent = upstream_saw.lock().unwrap().clone();
    for dropped in ["connection", "x-custom-hop", "proxy-authorization"] {
        assert!(
            sent.get(dropped).is_none(),
            "{dropped} must not be forwarded"
        );
    }
    assert_eq!(sent["x-keep"], "1");
    assert!(resp.headers().get("x-up-hop").is_none());
    assert_eq!(resp.headers()["x-up-keep"], "1");
}

/// Extensions API モックが受けた (path, 登録名 or 識別子ヘッダ)。
type ExtHeaders = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// `/register` は識別子 `ext-test` を払い出し、`/event/next` は
/// 1 回目に INVOKE、2 回目以降に SHUTDOWN を返す Extensions API モック。
async fn spawn_extensions_api(reason: &'static str) -> (String, Captured, ExtHeaders) {
    let seen: ExtHeaders = Arc::new(Mutex::new(Vec::new()));
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (addr, hits) = spawn_mock({
        let seen = seen.clone();
        move |_m, path, headers, _b| {
            let header = |k: &str| {
                headers
                    .get(k)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
            };
            if path == "/2020-01-01/extension/register" {
                seen.lock()
                    .unwrap()
                    .push((path.to_string(), header("lambda-extension-name")));
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("lambda-extension-identifier", "ext-test")
                    .body(boxed_full(Bytes::new()))
                    .unwrap();
            }
            if path == "/2020-01-01/extension/event/next" {
                seen.lock()
                    .unwrap()
                    .push((path.to_string(), header("lambda-extension-identifier")));
                let deadline = OffsetDateTime::now_utc().unix_timestamp() * 1000 + 60_000;
                if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return ok_body(json!({"eventType": "INVOKE", "deadlineMs": deadline}));
                }
                return ok_body(json!({
                    "eventType": "SHUTDOWN",
                    "shutdownReason": reason,
                    "deadlineMs": deadline
                }));
            }
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(boxed_full(Bytes::new()))
                .unwrap()
        }
    })
    .await;
    (addr, hits, seen)
}

fn register_body(hits: &Captured) -> Value {
    let (_, _, body) = captured(hits)
        .into_iter()
        .find(|(m, p, _)| m == "POST" && p == "/2020-01-01/extension/register")
        .expect("register request");
    serde_json::from_slice(&body).unwrap()
}

/// register 1 回 → INVOKE と SHUTDOWN の 2 回ポーリング、の順で
/// 登録名と識別子が正しく送られたこと。
fn assert_registered_then_polled_until_shutdown(seen: &ExtHeaders) {
    let seen = seen.lock().unwrap().clone();
    let next = "/2020-01-01/extension/event/next".to_string();
    let id = Some("ext-test".to_string());
    assert_eq!(
        seen,
        vec![
            (
                "/2020-01-01/extension/register".to_string(),
                Some("zankyo".to_string())
            ),
            (next.clone(), id.clone()),
            (next, id),
        ]
    );
}

#[tokio::test]
async fn external_agent_converts_staged_inflight_on_shutdown() {
    // 本番の timeout 捕捉経路: register → SHUTDOWN 受信 → .inflight を timeout 記録へ
    let (api_addr, api_hits, seen) = spawn_extensions_api("timeout").await;
    let (s3_addr, s3_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (recorder, spill_dir) = recorder_to(&s3_addr);
    recorder.stage_inflight(&Invocation {
        request_id: "req-agent".to_string(),
        event: json!({"token": "secret-token-value"}),
        encoding: EventEncoding::Json,
        invoked_at: OffsetDateTime::now_utc(),
    });

    assert!(zankyo::extension::run_agent(new_client(), api_addr, recorder).await);

    assert_eq!(
        register_body(&api_hits),
        json!({"events": ["INVOKE", "SHUTDOWN"]})
    );
    assert_registered_then_polled_until_shutdown(&seen);
    assert!(wait_for(2_000, || !captured(&s3_hits).is_empty()).await);
    let (method, path, body) = captured(&s3_hits).remove(0);
    assert_eq!(method, "PUT");
    assert!(path.contains("req-agent.json"));
    let rec: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec["failureType"], "timeout");
    assert_eq!(rec["errorContext"]["errorType"], "Timeout");
    assert!(!spill_dir.join("zankyo-req-agent.inflight").exists());
}

#[tokio::test]
async fn passthrough_agent_stays_registered_until_shutdown() {
    // 記録しない agent も登録して SHUTDOWN まで待つ。登録前や SHUTDOWN 前に
    // 終了すると、platform は終了コードに関係なく Init を失敗させるため。
    for (case, extra) in [
        (
            "disabled",
            vec![("ZANKYO_BUCKET", "b"), ("ZANKYO_DISABLED", "1")],
        ),
        ("no bucket", vec![]),
        (
            "config error",
            vec![("ZANKYO_BUCKET", "b"), ("ZANKYO_SCRUB_MODE", "bogus")],
        ),
    ] {
        let (api_addr, api_hits, seen) = spawn_extensions_api("spindown").await;
        let mut env: HashMap<String, String> = extra
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert("AWS_LAMBDA_RUNTIME_API".to_string(), api_addr);

        let code = tokio::time::timeout(
            Duration::from_secs(5),
            zankyo::orchestrate::run_agent_with_env(&env),
        )
        .await
        .unwrap_or_else(|_| panic!("{case}: agent must return after SHUTDOWN"));

        assert_eq!(code, 0, "{case}");
        // INVOKE は購読しない（呼び出しごとの往復を増やさない）
        assert_eq!(
            register_body(&api_hits),
            json!({"events": ["SHUTDOWN"]}),
            "{case}"
        );
        assert_registered_then_polled_until_shutdown(&seen);
    }
}

#[tokio::test]
async fn oversized_body_is_rejected_without_forwarding() {
    let (api_addr, api_hits) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let (s3_addr, _s3) = spawn_mock(|_m, _p, _h, _b| ok_empty()).await;
    let inflight = Arc::new(InFlight::new());
    let (recorder, _spill) = recorder_to(&s3_addr);
    let proxy = spawn_proxy(&api_addr, inflight, recorder).await;

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
