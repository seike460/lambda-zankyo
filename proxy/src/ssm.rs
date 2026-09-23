//! SSM Parameter Store からの設定 JSON 取得。
//! `ZANKYO_SSM_PARAM` 指定時だけ使う AWS 境界。取得失敗は呼び出し側が
//! warn に落として env 設定へ戻る（fail-open）。

use crate::error::{Result, ZankyoError};

/// SecureString 想定で復号付き取得。値が空のパラメータはエラーにする
/// （設定ミスの黙殺を防ぐ）。
pub async fn load_config_json(shared: &aws_config::SdkConfig, name: &str) -> Result<String> {
    let ssm = aws_sdk_ssm::Client::new(shared);
    let out = ssm
        .get_parameter()
        .name(name)
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| ZankyoError::Aws(e.to_string()))?;
    out.parameter()
        .and_then(|p| p.value().map(String::from))
        .ok_or_else(|| ZankyoError::Aws(format!("SSM parameter {name} has no value")))
}
