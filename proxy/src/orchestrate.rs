//! 起動シーケンスの配線。
//!
//! `Record` 確定後に必要なリソース（プロキシ listen、子プロセス、
//! extension 登録、spill 回復）を立ち上げ、子の終了コードを返す。
//! 各ステップの失敗は warn を残して passthrough へ落ちる（fail-open）。

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::extension;
use crate::inflight::InFlight;
use crate::proxy::{new_client, HttpClient, ProxyState};
use crate::runtime::{exit_code, passthrough, spawn_via_proxy};
use crate::setup::{self, RecordPlan, StartupPlan};
use crate::store::Recorder;

/// 実行本体。子プロセスの終了コードをそのまま返す。
/// どのステップで失敗しても zankyo を噛まない passthrough に落ちる。
pub async fn run(argv: &[OsString]) -> u8 {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let StartupPlan::Record(plan) = setup::resolve_plan(&env_map).await else {
        return passthrough(argv).await;
    };
    let RecordPlan {
        cfg,
        shared,
        upstream,
    } = *plan;

    let Some(listener) = bind_proxy_listener().await else {
        return passthrough(argv).await;
    };
    let Some(mut child) = spawn_child(argv, &listener) else {
        return passthrough(argv).await;
    };

    let register_timeout = std::time::Duration::from_millis(cfg.register_timeout_ms);
    let recorder = Arc::new(setup::build_recorder(&shared, cfg.clone(), &env_map));
    let inflight = Arc::new(InFlight::new());
    let client: HttpClient = new_client();

    // 前回の実行で残った spill があれば再送する。非同期・ベストエフォートで、
    // 起動経路を遅らせない。
    let rec = recorder.clone();
    tokio::spawn(async move { rec.recover_spills().await });

    let shutdown =
        start_extension(&client, &upstream, register_timeout, &inflight, &recorder).await;

    let state = Arc::new(ProxyState {
        upstream,
        client,
        inflight,
        recorder,
        cfg,
    });
    tokio::spawn(crate::proxy::serve(listener, state));

    wait_for_exit(&mut child, shutdown).await
}

/// 自分自身を Runtime API として listen する。
async fn bind_proxy_listener() -> Option<TcpListener> {
    match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => Some(l),
        Err(e) => {
            warn!(error = %e, "failed to bind proxy port; passthrough");
            None
        }
    }
}

/// listen ポートを確定し、子にそちらを向かせて起動する。
fn spawn_child(argv: &[OsString], listener: &TcpListener) -> Option<Child> {
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            warn!(error = %e, "no local addr; passthrough");
            return None;
        }
    };
    match spawn_via_proxy(argv, port) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(error = %e, "failed to spawn runtime via proxy; passthrough");
            None
        }
    }
}

/// extension 登録はベストエフォート: 失敗しても proxy 経由の
/// /error・/response 捕捉は残る（timeout 捕捉だけが失われる）。
/// 戻り値は SHUTDOWN 受信時に `true` で終わるタスクのハンドル。
async fn start_extension(
    client: &HttpClient,
    upstream: &str,
    register_timeout: std::time::Duration,
    inflight: &Arc<InFlight>,
    recorder: &Arc<Recorder>,
) -> Option<JoinHandle<bool>> {
    match extension::register(client, upstream, register_timeout).await {
        Ok(ext_id) => {
            info!("registered as external extension");
            let (c, api, inf, rec) = (
                client.clone(),
                upstream.to_string(),
                inflight.clone(),
                recorder.clone(),
            );
            Some(tokio::spawn(async move {
                extension::run_event_loop(c, api, ext_id, inf, rec).await
            }))
        }
        Err(e) => {
            warn!(error = %e, "extension register failed; timeout capture unavailable");
            None
        }
    }
}

/// 子の終了コードをそのまま返す。SHUTDOWN フラッシュ後に extension 側が
/// 先に終わる場合は正常終了（0）として抜ける。ただし extension が
/// SHUTDOWN 以外の理由（ポーリング断等）で終わった場合は、子プロセスの
/// 完了を待ち続ける — ここで抜けると関数実行中に子を殺してしまう。
async fn wait_for_exit(child: &mut Child, shutdown: Option<JoinHandle<bool>>) -> u8 {
    match shutdown {
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
