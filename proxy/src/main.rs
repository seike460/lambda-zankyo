//! zankyo — exec wrapper 兼 Runtime API proxy 兼 external extension。
//!
//! `AWS_LAMBDA_EXEC_WRAPPER` から起動され、argv に渡された本来の
//! ランタイム起動コマンドを子プロセスとして実行する。
//! 失敗時は原則 fail-open: zankyo 側の問題で関数本体を止めない。

mod config;
mod error;
mod extension;
mod inflight;
mod proxy;
mod record;
mod scrub;
mod store;

use crate::config::Config;
use crate::error::Result;
use crate::proxy::{new_client, HttpClient, ProxyState};
use crate::store::Recorder;
use aws_config::BehaviorVersion;
use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::process::Command;
use tracing::{info, warn};

const EX_USAGE: u8 = 64;
const EX_OSERR: u8 = 71;

fn init_tracing() {
    // ログは stderr へ。Lambda は wrapper の stdout/stderr を関数ログに混ぜる。
    // RUST_LOG で絞れるように env-filter を既定にする。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    init_tracing();
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    if argv.is_empty() {
        eprintln!("zankyo: exec wrapper for AWS Lambda.");
        eprintln!("usage: zankyo <runtime command> [args...]");
        eprintln!("(set AWS_LAMBDA_EXEC_WRAPPER=/opt/zankyo on the function)");
        return ExitCode::from(EX_USAGE);
    }
    ExitCode::from(run(argv).await)
}

async fn run(argv: Vec<OsString>) -> u8 {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let mut cfg = match Config::from_env_map(&env_map) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "config error; falling back to passthrough");
            return passthrough(&argv).await;
        }
    };

    let Some(upstream) = env_map.get("AWS_LAMBDA_RUNTIME_API").cloned() else {
        // Lambda 環境外（ローカル実行）。プロキシ先が無いので passthrough。
        return passthrough(&argv).await;
    };

    if cfg.disabled {
        info!("ZANKYO_DISABLED is set; running passthrough");
        return passthrough(&argv).await;
    }

    // SSM overlay: ZANKYO_SSM_PARAM 指定時はバケット未設定でも取得を試みる
    // （bucket が SSM 側だけに定義されるケースを許すため）。
    let needs_aws = cfg.ssm_param.is_some() || !cfg.bucket.is_empty();
    if !needs_aws {
        warn!("ZANKYO_BUCKET is not set; recording disabled (passthrough)");
        return passthrough(&argv).await;
    }

    let shared = aws_config::defaults(BehaviorVersion::latest()).load().await;
    if let Some(param) = cfg.ssm_param.clone() {
        match load_ssm(&shared, &param).await {
            Ok(json) => {
                if let Err(e) = cfg.overlay_ssm_json(&json) {
                    warn!(error = %e, param, "invalid SSM config JSON; using env config");
                }
            }
            Err(e) => warn!(error = %e, param, "SSM fetch failed; using env config"),
        }
    }
    if cfg.disabled {
        info!("ZANKYO_DISABLED via SSM; running passthrough");
        return passthrough(&argv).await;
    }
    if cfg.bucket.is_empty() {
        warn!("no ZANKYO_BUCKET after SSM overlay; recording disabled (passthrough)");
        return passthrough(&argv).await;
    }

    // 自分自身を Runtime API として listen し、子にはこちらを向かせる
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "failed to bind proxy port; passthrough");
            return passthrough(&argv).await;
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            warn!(error = %e, "no local addr; passthrough");
            return passthrough(&argv).await;
        }
    };

    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .env("AWS_LAMBDA_RUNTIME_API", format!("127.0.0.1:{port}"))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to spawn runtime via proxy; passthrough");
            return passthrough(&argv).await;
        }
    };

    let recorder = Arc::new(Recorder::new(
        aws_sdk_s3::Client::new(&shared),
        cfg,
        env_map
            .get("AWS_LAMBDA_FUNCTION_NAME")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()),
        env_map
            .get("AWS_LAMBDA_FUNCTION_VERSION")
            .cloned()
            .unwrap_or_else(|| "$LATEST".to_string()),
    ));
    let inflight = Arc::new(crate::inflight::InFlight::new());
    let client: HttpClient = new_client();

    // extension 登録はベストエフォート: 失敗しても proxy 経由の
    // /error・/response 捕捉は残る（timeout 捕捉だけが失われる）
    let ext_handle = match extension::register(&client, &upstream).await {
        Ok(ext_id) => {
            info!("registered as external extension");
            let (c, api, inf, rec) = (
                client.clone(),
                upstream.clone(),
                inflight.clone(),
                recorder.clone(),
            );
            Some(tokio::spawn(async move {
                extension::run_event_loop(c, api, ext_id, inf, rec).await;
            }))
        }
        Err(e) => {
            warn!(error = %e, "extension register failed; timeout capture unavailable");
            None
        }
    };

    let state = Arc::new(ProxyState {
        upstream,
        client,
        inflight,
        recorder,
    });
    tokio::spawn(proxy::serve(listener, state));

    // 子の終了コードをそのまま返す。SHUTDOWN フラッシュ後に extension 側が
    // 先に終わる場合は正常終了（0）として抜ける。
    match ext_handle {
        Some(h) => tokio::select! {
            status = child.wait() => exit_code(status),
            _ = h => 0,
        },
        None => exit_code(child.wait().await),
    }
}

fn exit_code(status: std::io::Result<std::process::ExitStatus>) -> u8 {
    match status {
        Ok(s) => s.code().map(|c| c as u8).unwrap_or(1),
        Err(_) => 1,
    }
}

/// zankyo を噛ませず子プロセスだけ起動する（fail-open 経路）。
async fn passthrough(argv: &[OsString]) -> u8 {
    match Command::new(&argv[0]).args(&argv[1..]).status().await {
        Ok(s) => s.code().map(|c| c as u8).unwrap_or(1),
        Err(e) => {
            warn!(error = %e, "failed to spawn child process");
            EX_OSERR
        }
    }
}

async fn load_ssm(shared: &aws_config::SdkConfig, name: &str) -> Result<String> {
    let ssm = aws_sdk_ssm::Client::new(shared);
    let out = ssm
        .get_parameter()
        .name(name)
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| crate::error::ZankyoError::Aws(e.to_string()))?;
    out.parameter()
        .and_then(|p| p.value().map(String::from))
        .ok_or_else(|| crate::error::ZankyoError::Aws(format!("SSM parameter {name} has no value")))
}
