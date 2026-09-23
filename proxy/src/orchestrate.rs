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
use crate::proxy::{new_client, HttpClient, PendingSaves, ProxyState};
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

    let pending: PendingSaves = tokio::sync::Mutex::new(crate::proxy::Pending::new());
    // 前回の実行で残った spill があれば再送する。非同期・ベストエフォートで、
    // 起動経路を遅らせない。初回再送は pending へ積み、終了時の drain に
    // 含める（早期終了時に PUT が途中で切られないようにする）。
    {
        let rec = recorder.clone();
        pending
            .lock()
            .await
            .set
            .spawn(async move { rec.recover_spills().await });
    }
    // 生存中も定期再送する。S3 の一時障害が回復した時点で
    // /tmp を空に戻し、溜まったままの状態を放置しない。
    {
        let rec = recorder.clone();
        let retry = std::time::Duration::from_millis(cfg.spill_retry_ms);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(retry);
            tick.tick().await; // interval の初回即時発火は初期再送が担うので捨てる
            loop {
                tick.tick().await;
                rec.recover_spills().await;
            }
        });
    }

    let drain_budget = std::time::Duration::from_millis(cfg.put_timeout_ms.saturating_add(1_000));
    let state = Arc::new(ProxyState {
        upstream: upstream.clone(),
        client: client.clone(),
        inflight,
        recorder,
        cfg,
        pending,
        active: std::sync::atomic::AtomicUsize::new(0),
    });
    let server = tokio::spawn(crate::proxy::serve(listener, state.clone()));

    // extension 登録は proxy serve と並行して行う。
    // register を先に await すると、ハングした場合に子の初回 /next が
    // register_timeout の分だけ遅れる。
    let shutdown = tokio::spawn(start_extension(
        client,
        upstream,
        register_timeout,
        state.inflight.clone(),
        state.recorder.clone(),
    ));

    let code = wait_for_exit(&mut child, shutdown).await;
    // 新規接続を止めてから、残ったハンドラと save を待つ。
    // 応答は返したが save が未完了のレコードを、runtime 解体前に
    // 一定時間だけ待って拾い切る（init_error は POST 直後に子が
    // 終了するため、ここを設けないと構造的に記録が失われる）。
    server.abort();
    drain_pending(&state, drain_budget).await;
    code
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
/// SHUTDOWN 受信時に `true`、それ以外で終われば `false` を返す。
async fn start_extension(
    client: HttpClient,
    upstream: String,
    register_timeout: std::time::Duration,
    inflight: Arc<InFlight>,
    recorder: Arc<Recorder>,
) -> bool {
    match extension::register(&client, &upstream, register_timeout).await {
        Ok(ext_id) => {
            info!("registered as external extension");
            extension::run_event_loop(client, upstream, ext_id, inflight, recorder).await
        }
        Err(e) => {
            warn!(error = %e, "extension register failed; timeout capture unavailable");
            false
        }
    }
}

/// 子の終了コードをそのまま返す。SHUTDOWN フラッシュ後に extension 側が
/// 先に終わる場合は正常終了（0）として抜ける。ただし extension が
/// SHUTDOWN 以外の理由（ポーリング断等）で終わった場合は、子プロセスの
/// 完了を待ち続ける — ここで抜けると関数実行中に子を殺してしまう。
async fn wait_for_exit(child: &mut Child, shutdown: JoinHandle<bool>) -> u8 {
    tokio::select! {
        status = child.wait() => exit_code(status),
        res = shutdown => match res {
            Ok(true) => 0,
            Ok(false) | Err(_) => {
                warn!("extension loop ended without shutdown; still waiting on runtime");
                exit_code(child.wait().await)
            }
        },
    }
}

/// 積まれた save タスクが空になるまで待つ。全体で `budget` まで —
/// S3 が応答しない環境で子プロセスの終了を無制限に遅らせない。
/// pending を closed にしてから join するので、drain 中に到着した
/// save は JoinSet ではなくハンドラ側のインライン実行になり、
/// それらも `active` が 0 になるまで待ち合わせる。
async fn drain_pending(state: &ProxyState, budget: std::time::Duration) {
    let drain = async {
        {
            let mut p = state.pending.lock().await;
            p.closed = true;
            while p.set.join_next().await.is_some() {}
        }
        // closed 以後にインライン化した save はハンドラの仕事として
        // 残っているので、ハンドラが居なくなるまで待つ。
        while state.active.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    if tokio::time::timeout(budget, drain).await.is_err() {
        warn!("pending record saves did not finish before exit");
    }
}
