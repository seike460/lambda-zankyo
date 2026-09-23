//! 起動時の配線: SSM overlay と Recorder 構築。
//! main.rs から切り離し、env マップからのメタデータ解決を
//! 単体テスト可能にする。

use crate::config::Config;
use crate::store::Recorder;
use std::collections::HashMap;
use tracing::warn;

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
}
