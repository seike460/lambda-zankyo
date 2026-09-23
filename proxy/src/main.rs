//! zankyo — exec wrapper 兼 Runtime API proxy 兼 external extension。
//!
//! `AWS_LAMBDA_EXEC_WRAPPER` から起動され、argv に渡された本来の
//! ランタイム起動コマンドを子プロセスとして実行する。
//! 失敗時は原則 fail-open: zankyo 側の問題で関数本体を止めない。
//! このファイルは配線と分岐だけを担い、各処理はライブラリモジュールへ委譲する。

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{info, warn};
use zankyo::inflight::InFlight;
use zankyo::proxy::{new_client, HttpClient, ProxyState};
use zankyo::runtime::{exit_code, passthrough, spawn_via_proxy};
use zankyo::setup::StartupPlan;

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
    // 起動形態の判定（設定解決・SSM overlay・fail-open 分岐）は setup 側。
    // ここでは Record 確定後の配線だけを直線で書く。
    let StartupPlan::Record(plan) = zankyo::setup::resolve_plan(&env_map).await else {
        return passthrough(&argv).await;
    };
    let zankyo::setup::RecordPlan {
        cfg,
        shared,
        upstream,
    } = *plan;

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
                zankyo::extension::run_event_loop(c, api, ext_id, inf, rec).await
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
    // 先に終わる場合は正常終了（0）として抜ける。ただし extension が
    // SHUTDOWN 以外の理由（ポーリング断等）で終わった場合は、子プロセスの
    // 完了を待ち続ける — ここで抜けると関数実行中に子を殺してしまう。
    match ext_handle {
        Some(h) => tokio::select! {
            status = child.wait() => exit_code(status),
            res = h => match res {
                Ok(true) => 0,
                Ok(false) | Err(_) => {
                    warn!("extension loop ended without shutdown; still waiting on runtime");
                    exit_code(child.wait().await)
                }
            },
        },
        None => exit_code(child.wait().await),
    }
}
