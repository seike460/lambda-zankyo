//! zankyo — exec wrapper 兼 Runtime API proxy 兼 external extension。
//!
//! `AWS_LAMBDA_EXEC_WRAPPER` から起動され、argv に渡された本来の
//! ランタイム起動コマンドを子プロセスとして実行する。
//! 失敗時は原則 fail-open: zankyo 側の問題で関数本体を止めない。
//! このファイルは配線と分岐だけを担い、各処理はライブラリモジュールへ委譲する。

use aws_config::BehaviorVersion;
use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{info, warn};
use zankyo::config::Config;
use zankyo::inflight::InFlight;
use zankyo::proxy::{new_client, HttpClient, ProxyState};
use zankyo::runtime::{exit_code, passthrough, spawn_via_proxy};

const EX_USAGE: u8 = 64;

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
    zankyo::setup::apply_ssm_overlay(&shared, &mut cfg).await;
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

    let mut child = match spawn_via_proxy(&argv, port) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to spawn runtime via proxy; passthrough");
            return passthrough(&argv).await;
        }
    };

    let recorder = Arc::new(zankyo::setup::build_recorder(&shared, cfg, &env_map));
    let inflight = Arc::new(InFlight::new());
    let client: HttpClient = new_client();

    // 前回の実行で残った spill があれば再送する。非同期・ベストエフォートで、
    // 起動経路を遅らせない。
    {
        let rec = recorder.clone();
        tokio::spawn(async move { rec.recover_spills().await });
    }

    // extension 登録はベストエフォート: 失敗しても proxy 経由の
    // /error・/response 捕捉は残る（timeout 捕捉だけが失われる）
    let ext_handle = match zankyo::extension::register(&client, &upstream).await {
        Ok(ext_id) => {
            info!("registered as external extension");
            let (c, api, inf, rec) = (
                client.clone(),
                upstream.clone(),
                inflight.clone(),
                recorder.clone(),
            );
            Some(tokio::spawn(async move {
                zankyo::extension::run_event_loop(c, api, ext_id, inf, rec).await;
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
    tokio::spawn(zankyo::proxy::serve(listener, state));

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
