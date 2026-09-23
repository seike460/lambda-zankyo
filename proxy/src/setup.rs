//! 起動時の配線: SSM overlay と Recorder 構築。
//! main.rs から切り離し、env マップからのメタデータ解決を
//! 単体テスト可能にする。

use crate::config::Config;
use crate::store::Recorder;
use aws_config::BehaviorVersion;
use std::collections::HashMap;
use tracing::{info, warn};

/// 起動判定の結果。`Passthrough` なら zankyo を噛まず子だけ起動する
/// （fail-open）。`Record` は記録配線に必要なものを全部持つ。
pub enum StartupPlan {
    Passthrough,
    Record(Box<RecordPlan>),
}

/// 記録モードで起動するために必要な設定一式。
pub struct RecordPlan {
    pub cfg: Config,
    pub shared: aws_config::SdkConfig,
    pub upstream: String,
}

/// env 設定 + SSM overlay から起動形態を決める。
/// 記録を続行できない理由はすべてここで warn として残し、
/// 呼び出し側は enum の分岐だけを見ればよい。
pub async fn resolve_plan(env_map: &HashMap<String, String>) -> StartupPlan {
    let mut cfg = match Config::from_env_map(env_map) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "config error; falling back to passthrough");
            return StartupPlan::Passthrough;
        }
    };
    let Some(upstream) = env_map.get("AWS_LAMBDA_RUNTIME_API").cloned() else {
        // Lambda 環境外（ローカル実行）。プロキシ先が無いので passthrough。
        return StartupPlan::Passthrough;
    };
    if cfg.disabled {
        info!("ZANKYO_DISABLED is set; running passthrough");
        return StartupPlan::Passthrough;
    }
    // SSM overlay: ZANKYO_SSM_PARAM 指定時はバケット未設定でも取得を試みる
    // （bucket が SSM 側だけに定義されるケースを許すため）。
    if cfg.ssm_param.is_none() && cfg.bucket.is_empty() {
        warn!("ZANKYO_BUCKET is not set; recording disabled (passthrough)");
        return StartupPlan::Passthrough;
    }
    let shared = aws_config::defaults(BehaviorVersion::latest()).load().await;
    apply_ssm_overlay(&shared, &mut cfg).await;
    if cfg.disabled {
        info!("ZANKYO_DISABLED via SSM; running passthrough");
        return StartupPlan::Passthrough;
    }
    if cfg.bucket.is_empty() {
        warn!("no ZANKYO_BUCKET after SSM overlay; recording disabled (passthrough)");
        return StartupPlan::Passthrough;
    }
    StartupPlan::Record(Box::new(RecordPlan {
        cfg,
        shared,
        upstream,
    }))
}

/// `ZANKYO_SSM_PARAM` があれば SSM の設定 JSON を env 設定へ重ねる。
/// 取得・パースの失敗は warn で落として env 設定のまま進む（fail-open）。
pub async fn apply_ssm_overlay(shared: &aws_config::SdkConfig, cfg: &mut Config) {
    let Some(param) = cfg.ssm_param.clone() else {
        return;
    };
    match crate::ssm::load_config_json(shared, &param).await {
        Ok(json) => {
            if let Err(e) = cfg.overlay_ssm_json(&json) {
                warn!(error = %e, param, "invalid SSM config JSON; using env config");
            }
        }
        Err(e) => warn!(error = %e, param, "SSM fetch failed; using env config"),
    }
}

/// 関数メタデータと設定から Recorder を組み立てる。
/// Lambda 環境変数が欠けている場合の既定値はここで一元化する。
pub fn build_recorder(
    shared: &aws_config::SdkConfig,
    cfg: Config,
    env_map: &HashMap<String, String>,
) -> Recorder {
    Recorder::new(
        aws_sdk_s3::Client::new(shared),
        cfg,
        function_name(env_map),
        env_map
            .get("AWS_LAMBDA_FUNCTION_VERSION")
            .cloned()
            .unwrap_or_else(|| "$LATEST".to_string()),
    )
}

/// `AWS_LAMBDA_FUNCTION_NAME` が無い環境（ローカル実行等）の既定値。
fn function_name(env_map: &HashMap<String, String>) -> String {
    env_map
        .get("AWS_LAMBDA_FUNCTION_NAME")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_name_defaults_to_unknown() {
        let env = HashMap::new();
        assert_eq!(function_name(&env), "unknown");
    }

    #[test]
    fn function_name_reads_lambda_env() {
        let env = HashMap::from([("AWS_LAMBDA_FUNCTION_NAME".to_string(), "my-fn".to_string())]);
        assert_eq!(function_name(&env), "my-fn");
    }

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn plan_is_passthrough_outside_lambda() {
        // AWS_LAMBDA_RUNTIME_API が無い環境では AWS 設定に触れず passthrough
        let env = env_with(&[("ZANKYO_BUCKET", "b")]);
        assert!(matches!(resolve_plan(&env).await, StartupPlan::Passthrough));
    }

    #[tokio::test]
    async fn plan_is_passthrough_when_disabled() {
        let env = env_with(&[
            ("AWS_LAMBDA_RUNTIME_API", "127.0.0.1:9001"),
            ("ZANKYO_BUCKET", "b"),
            ("ZANKYO_DISABLED", "1"),
        ]);
        assert!(matches!(resolve_plan(&env).await, StartupPlan::Passthrough));
    }

    #[tokio::test]
    async fn plan_is_passthrough_without_bucket_or_ssm() {
        // バケットも SSM パラメータも無い → AWS SDK に触れる前に passthrough
        let env = env_with(&[("AWS_LAMBDA_RUNTIME_API", "127.0.0.1:9001")]);
        assert!(matches!(resolve_plan(&env).await, StartupPlan::Passthrough));
    }
}
