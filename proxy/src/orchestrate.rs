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
use tracing::{debug, info, warn};

use crate::extension;
use crate::inflight::InFlight;
use crate::proxy::{new_client, HttpClient, PendingSaves, ProxyState};
use crate::runtime::{exit_code, passthrough, spawn_via_proxy};
use crate::setup::{self, RecordPlan, StartupPlan};
use crate::store::Recorder;

/// drain 用の時間枠に加える余白。put_timeout ちょうどだと
/// 最後の 1 件が送信完了前に打ち切られうるため。
const DRAIN_SLACK_MS: u64 = 1_000;
/// 子終了後に extension の SHUTDOWN 処理を待つ猶予の上限。
/// Lambda の sandbox 凍結までに残る時間は限られるため上限を設ける。
const SHUTDOWN_GRACE_CAP_MS: u64 = 1_000;
/// layer が配置する external extension の起動ファイル。
/// 存在する環境では platform が別プロセスで agent を起動するため、
/// このプロセスからの internal register は不要（拒否される）。
const EXTERNAL_EXT_PATH: &str = "/opt/extensions/zankyo";

/// external extension プロセスのエントリ。
/// `/opt/extensions/zankyo` から platform が argv なしで起動する
/// （main.rs が空 argv をここへ振り分ける）。proxy と別プロセスの
/// ため Runtime API へ自前で register し、SHUTDOWN で inflight
/// ステージを timeout 記録へ変換する。
/// passthrough でもすぐには終了しない: SHUTDOWN 前に extension が
/// 終わると、終了コードに関係なく platform は Init を失敗させる。
/// 記録しない場合も登録して SHUTDOWN まで待つ（fail-open）。
/// SHUTDOWN の前に戻るのは、登録を拒否されたときと、Extensions API が
/// 回復不能になったときだけ。その場合は 1 を返し、正常終了と区別する。
pub async fn run_agent() -> u8 {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    run_agent_with_env(&env_map).await
}

/// `run_agent` の本体。env を引数で受け、起動判定からの配線を
/// プロセスの環境変数に触れずに検証できるようにする。
pub async fn run_agent_with_env(env_map: &HashMap<String, String>) -> u8 {
    let completed = match setup::resolve_plan(env_map).await {
        StartupPlan::Record(plan) => {
            let RecordPlan {
                cfg,
                shared,
                upstream,
            } = *plan;
            let recorder = Arc::new(setup::build_recorder(&shared, cfg, env_map));
            extension::run_agent(new_client(), upstream, recorder).await
        }
        StartupPlan::Passthrough => match env_map.get("AWS_LAMBDA_RUNTIME_API") {
            Some(upstream) => {
                extension::run_passthrough_agent(new_client(), upstream.clone()).await
            }
            // main.rs はこの変数があるときだけ agent を起動する。無ければ待つ相手もいない
            None => true,
        },
    };
    if completed {
        0
    } else {
        1
    }
}

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
    // 順序不変条件: 回収する inflight 残滓の一覧は serve 開始前に確定する
    // （startup_recovery）。稼働後に列挙すると進行中の呼び出しを未完と誤認する。
    {
        let recovery = startup_recovery(recorder.clone());
        pending.lock().await.set.spawn(recovery);
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

    let drain_budget =
        std::time::Duration::from_millis(cfg.put_timeout_ms.saturating_add(DRAIN_SLACK_MS));
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
    // layer に /opt/extensions/zankyo が含まれる環境では、platform が
    // external extension として別プロセスで agent を起動する。
    // internal register は SHUTDOWN を拒否されるため、その場合は省く。
    // SHUTDOWN フラッシュは agent が担い、その完了はこのプロセスから
    // 観測できないので、待つハンドルも持たない（None）。
    let has_external_ext = std::path::Path::new(EXTERNAL_EXT_PATH).exists();
    let shutdown = if has_external_ext {
        info!("external extension detected; internal register skipped");
        None
    } else {
        Some(tokio::spawn(start_extension(
            client,
            upstream,
            register_timeout,
            state.inflight.clone(),
            state.recorder.clone(),
        )))
    };

    let (code, pending_shutdown) = wait_for_exit(&mut child, shutdown).await;
    // 子が先に落ちた場合、Lambda がランタイム死亡を検知して SHUTDOWN を
    // 配信するまで数十〜数百 ms ある。同じプロセスの extension が
    // in-flight を持ったまま生きているなら、bounded に待って
    // extension 側の正式なフラッシュ（spill+PUT）に任せる。
    if let Some(h) = pending_shutdown {
        if !state.inflight.is_empty() {
            let grace = state.cfg.flush_budget_ms.min(SHUTDOWN_GRACE_CAP_MS);
            // 猶予切れは想定内: extension 側がまだ処理中なら
            // そのまま drain_and_claim 側で拾う。
            if tokio::time::timeout(std::time::Duration::from_millis(grace), h)
                .await
                .is_err()
            {
                debug!("shutdown grace elapsed; spilling remaining in-flight");
            }
        }
    }
    // 残った呼び出し（ランタイムクラッシュ・SHUTDOWN 未到着・
    // extension 死亡）は /tmp へ同期退避する。
    // PUT は試さない — プロセス終了が目前で、次回 init の
    // recover_spills が回収する方が確実。
    // drain_and_claim で SHUTDOWN フラッシュとの取り合いも一意に決まる。
    for inv in state.inflight.drain_and_claim() {
        // None はシリアライズ失敗＝このレコードは残せない。warn に残す。
        if state.recorder.stage_timeout(&inv, None).is_none() {
            warn!(request_id = %inv.request_id, "failed to stage timed-out record");
        }
    }
    // 新規接続を止めてから、残ったハンドラと save を待つ。
    // 応答は返したが save が未完了のレコードを、runtime 解体前に
    // 一定時間だけ待って拾い切る（init_error は POST 直後に子が
    // 終了するため、ここを設けないと構造的に記録が失われる）。
    server.abort();
    crate::proxy::drain_pending(&state, drain_budget).await;
    code
}

/// 起動時の回収処理を作る。先に inflight 残滓を変換する —
/// 「応答なく環境が畳まれた呼び出し」を timeout レコード化してから、
/// 溜まった spill 全件を再送する順。
/// 回収する `.inflight` の一覧は、この関数を呼んだ時点で確定させる。
/// 返す Future の中で列挙すると、serve 開始後に届いた初回 `/next` の
/// ステージまで拾い、実行中の呼び出しを timeout と誤記録しうる。
/// そのため serve を始める前に呼ぶ。
pub fn startup_recovery(recorder: Arc<Recorder>) -> impl std::future::Future<Output = ()> + Send {
    let leftovers = recorder.pending_inflights();
    async move {
        // 前環境の残滓は shutdown reason が分からない（None=unknown）
        recorder.recover_inflight_paths(leftovers, None).await;
        recorder.recover_spills().await;
    }
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

/// exec wrapper プロセス内からの internal register。
/// `/opt/extensions/zankyo` を含まない独自 Layer 向けのフォールバック。
/// AWS は internal extension の SHUTDOWN 購読を認めないため、
/// 実環境では register が拒否されうる。
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
    let retry = std::time::Duration::from_millis(recorder.config().ext_retry_ms);
    match extension::register(&client, &upstream, register_timeout, retry).await {
        Ok(ext_id) => {
            info!("registered as internal extension");
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
/// SHUTDOWN 以外の理由（登録の拒否・Extensions API の回復不能なエラー等）で
/// 終わった場合は、子プロセスの完了を待ち続ける — ここで抜けると
/// 関数実行中に子を殺してしまう。
/// `shutdown` が None（プロセス内に extension が無い）なら子だけを待つ。
/// 戻り値の Some(handle) は「子が先に終わり extension が生存中」の場合で、
/// 呼び出し側が SHUTDOWN 到着を短く待つ判断に使う。
async fn wait_for_exit(
    child: &mut Child,
    shutdown: Option<JoinHandle<bool>>,
) -> (u8, Option<JoinHandle<bool>>) {
    let Some(mut shutdown) = shutdown else {
        return (exit_code(child.wait().await), None);
    };
    tokio::select! {
        status = child.wait() => (exit_code(status), Some(shutdown)),
        res = &mut shutdown => match res {
            Ok(true) => (0, None),
            Ok(false) | Err(_) => {
                warn!("extension loop ended without shutdown; still waiting on runtime");
                (exit_code(child.wait().await), None)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exiting_child(code: u8) -> Child {
        tokio::process::Command::new("sh")
            .args(["-c", &format!("exit {code}")])
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn wait_for_exit_without_extension_returns_no_handle() {
        // external extension 構成: 待つハンドルが無いので grace 待ちも起きない
        let mut child = exiting_child(3);
        let (code, pending) = wait_for_exit(&mut child, None).await;
        assert_eq!(code, 3);
        assert!(pending.is_none());
    }

    #[tokio::test]
    async fn wait_for_exit_keeps_live_extension_handle() {
        let mut child = exiting_child(3);
        let ext = tokio::spawn(std::future::pending::<bool>());
        let (code, pending) = wait_for_exit(&mut child, Some(ext)).await;
        assert_eq!(code, 3);
        let h = pending.expect("live in-process extension handle");
        h.abort();
    }
}
