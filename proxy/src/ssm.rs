//! SSM Parameter Store からの設定 JSON 取得。
//! `ZANKYO_SSM_PARAM` 指定時だけ使う AWS 境界。取得失敗は呼び出し側が
//! warn に落として env 設定へ戻る（fail-open）。

use crate::error::{Result, ZankyoError};
use std::time::Duration;

/// init 経路を遅らせないための取得上限。
const GET_TIMEOUT: Duration = Duration::from_secs(10);

/// SecureString 想定で復号付き取得。値が空のパラメータはエラーにする
/// （設定ミスの黙殺を防ぐ）。呼び出しはタイムアウト付き。
pub async fn load_config_json(shared: &aws_config::SdkConfig, name: &str) -> Result<String> {
    let ssm = aws_sdk_ssm::Client::new(shared);
    let out = tokio::time::timeout(
        GET_TIMEOUT,
        ssm.get_parameter().name(name).with_decryption(true).send(),
    )
    .await
    .map_err(|_| ZankyoError::Aws(format!("SSM get_parameter {name} timed out")))?
    .map_err(|e| ZankyoError::Aws(e.to_string()))?;
    out.parameter()
        .and_then(|p| p.value().map(String::from))
        .ok_or_else(|| ZankyoError::Aws(format!("SSM parameter {name} has no value")))
}
