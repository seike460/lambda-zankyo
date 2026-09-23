//! 設定の解決。
//!
//! 環境変数が既定の供給元。`ZANKYO_SSM_PARAM` が指定されている場合は
//! SSM Parameter Store の JSON を読み、同じキーがあれば env を上書きする
//! （複数関数で設定を一元管理したい利用者のため）。SSM 側のスキーマは
//! env 名と同じキーを持つフラットな JSON オブジェクト。

use crate::error::{Result, ZankyoError};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};

pub const DEFAULT_MAX_EVENT_KB: usize = 256;
pub const DEFAULT_FLUSH_BUDGET_MS: u64 = 1_200;
pub const DEFAULT_PUT_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_SPILL_DIR: &str = "/tmp/zankyo";
/// spill dir に残すレコードの最大数。S3 が届かない状態が続いても
/// /tmp を使い尽くさないよう、古いものから捨てる。
pub const DEFAULT_SPILL_MAX_FILES: usize = 64;
/// Runtime API が受け付けるボディの上限（KiB）。Lambda の同期
/// ペイロード上限 6MiB に余裕を持たせた既定。
pub const DEFAULT_MAX_BODY_KB: usize = 8192;
/// Extensions API のイベントボディ上限（KiB）。通知はメタデータだけ
/// なので 1MiB で十分。
pub const DEFAULT_EXT_BODY_KB: usize = 1024;
/// extension 登録の上限時間（ms）。
pub const DEFAULT_REGISTER_TIMEOUT_MS: u64 = 10_000;
/// event/next ポーリング失敗時の再試行間隔（ms）。
pub const DEFAULT_EXT_RETRY_MS: u64 = 500;
/// ポーリング連続失敗の上限。既定では 500ms × 120 ≒ 60 秒失敗が
/// 続いたら Extensions API の障害とみなしてループを抜ける。
pub const DEFAULT_EXT_MAX_POLL_FAILURES: u32 = 120;
/// SSM get_parameter の上限時間（ms）。
pub const DEFAULT_SSM_TIMEOUT_MS: u64 = 10_000;
/// `/next` 以外の上流転送の上限時間（ms）。localhost 上の Runtime API が
/// 60 秒応えない状況は実行環境の異常とみなす。
pub const DEFAULT_FORWARD_TIMEOUT_MS: u64 = 60_000;
/// spill 再送の間隔（ms）。起動時だけでなく生存中も定期的に
/// /tmp を空に戻し、S3 の一時障害からの回復を早める。
pub const DEFAULT_SPILL_RETRY_MS: u64 = 60_000;
/// spill ファイルの有効期間（秒）。sandbox の /tmp は短命なので
/// 実際にはほぼ発動しないが、古すぎるレコードの遅れ再送で
/// 記録順を混乱させないための上限。
pub const DEFAULT_SPILL_MAX_AGE_SECS: u64 = 604_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrubMode {
    /// 形状保持マスク（既定）。再現性を損なわないよう先頭文字などを残す。
    Mask,
    /// HMAC による擬似名化。値の同一性は保ちつつ中身を秘匿する。
    Hash,
    /// 明示的に scrub を切る。設定ミス防止のため文字列 "off" のみで有効。
    Off,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// 失敗レコードの保存先バケット。空は「未設定=記録しない」の意味で、
    /// wrapper を passthrough に倒す判定に使う。
    pub bucket: String,
    pub kms_key: Option<String>,
    pub ssm_param: Option<String>,
    /// 既定 denylist に追加するフィールド名（正規化済み）。
    pub scrub_fields: BTreeSet<String>,
    pub scrub_mode: ScrubMode,
    pub max_event_kb: usize,
    /// SHUTDOWN 検知後に S3 フラッシュへ使える時間の上限。
    pub flush_budget_ms: u64,
    /// 通常経路の PutObject 上限時間。呼び出し経路を遅らせないため短め。
    pub put_timeout_ms: u64,
    /// S3 失敗時・SHUTDOWN 時のローカル退避先。
    pub spill_dir: String,
    /// spill dir に保持するファイル数の上限。超過分は古いものから破棄。
    pub spill_max_files: usize,
    /// spill 再送を試みる間隔（ms）。
    pub spill_retry_ms: u64,
    /// spill ファイルの有効期間（秒）。超過分は再送せず破棄する。
    pub spill_max_age_secs: u64,
    /// Runtime API 経由で受け付けるボディの上限（KiB）。
    pub max_body_kb: usize,
    /// Extensions API イベントボディの上限（KiB）。
    pub ext_body_kb: usize,
    /// extension 登録リクエストの上限時間（ms）。
    pub register_timeout_ms: u64,
    /// event/next ポーリング失敗時の再試行間隔（ms）。
    pub ext_retry_ms: u64,
    /// ポーリング連続失敗の上限。超えると extension ループを抜ける。
    pub ext_max_poll_failures: u32,
    /// SSM get_parameter の上限時間（ms）。
    pub ssm_timeout_ms: u64,
    /// `/next` 以外の上流転送の上限時間（ms）。
    pub forward_timeout_ms: u64,
    pub disabled: bool,
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn parse_mode(v: &str) -> Result<ScrubMode> {
    match v.to_ascii_lowercase().as_str() {
        "mask" => Ok(ScrubMode::Mask),
        "hash" => Ok(ScrubMode::Hash),
        "off" => Ok(ScrubMode::Off),
        other => Err(ZankyoError::Config(format!(
            "invalid ZANKYO_SCRUB_MODE {other:?} (expected mask|hash|off)"
        ))),
    }
}

fn parse_fields(v: &str) -> BTreeSet<String> {
    v.split(',')
        .map(crate::scrub::normalize_field)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 0 を拒否する正整数パーサ。0 値の上限・タイムアウトは設定ミスで
/// 関数本体まで壊す（MAX_BODY_KB=0 で全 POST が 413 になる等）。
fn parse_usize(key: &str, v: &str) -> Result<usize> {
    parse_u64(key, v).map(|n| n as usize)
}

fn parse_u64(key: &str, v: &str) -> Result<u64> {
    match v.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ZankyoError::Config(format!(
            "{key} must be a positive integer, got {v:?}"
        ))),
    }
}

fn parse_u32(key: &str, v: &str) -> Result<u32> {
    match v.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ZankyoError::Config(format!(
            "{key} must be a positive integer, got {v:?}"
        ))),
    }
}

impl Config {
    /// env 相当のキー集合から設定を構築する。バケット未設定でも Err にしない:
    /// その場合は wrapper が passthrough に倒れ、関数本体を止めない。
    pub fn from_env_map(env: &HashMap<String, String>) -> Result<Self> {
        let get = |k: &str| env.get(k).map(String::as_str);
        let mut cfg = Config {
            bucket: get("ZANKYO_BUCKET").unwrap_or_default().to_string(),
            kms_key: get("ZANKYO_KMS_KEY").map(String::from),
            ssm_param: get("ZANKYO_SSM_PARAM").map(String::from),
            scrub_fields: BTreeSet::new(),
            scrub_mode: ScrubMode::Mask,
            max_event_kb: DEFAULT_MAX_EVENT_KB,
            flush_budget_ms: DEFAULT_FLUSH_BUDGET_MS,
            put_timeout_ms: DEFAULT_PUT_TIMEOUT_MS,
            spill_dir: DEFAULT_SPILL_DIR.to_string(),
            spill_max_files: DEFAULT_SPILL_MAX_FILES,
            spill_retry_ms: DEFAULT_SPILL_RETRY_MS,
            spill_max_age_secs: DEFAULT_SPILL_MAX_AGE_SECS,
            max_body_kb: DEFAULT_MAX_BODY_KB,
            ext_body_kb: DEFAULT_EXT_BODY_KB,
            register_timeout_ms: DEFAULT_REGISTER_TIMEOUT_MS,
            ext_retry_ms: DEFAULT_EXT_RETRY_MS,
            ext_max_poll_failures: DEFAULT_EXT_MAX_POLL_FAILURES,
            ssm_timeout_ms: DEFAULT_SSM_TIMEOUT_MS,
            forward_timeout_ms: DEFAULT_FORWARD_TIMEOUT_MS,
            disabled: false,
        };
        if let Some(v) = get("ZANKYO_SCRUB_FIELDS") {
            cfg.scrub_fields = parse_fields(v);
        }
        if let Some(v) = get("ZANKYO_SCRUB_MODE") {
            cfg.scrub_mode = parse_mode(v)?;
        }
        if let Some(v) = get("ZANKYO_MAX_EVENT_KB") {
            cfg.max_event_kb = parse_usize("ZANKYO_MAX_EVENT_KB", v)?;
        }
        if let Some(v) = get("ZANKYO_FLUSH_BUDGET_MS") {
            cfg.flush_budget_ms = parse_u64("ZANKYO_FLUSH_BUDGET_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_PUT_TIMEOUT_MS") {
            cfg.put_timeout_ms = parse_u64("ZANKYO_PUT_TIMEOUT_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_SPILL_DIR") {
            cfg.spill_dir = v.to_string();
        }
        if let Some(v) = get("ZANKYO_SPILL_MAX_FILES") {
            cfg.spill_max_files = parse_usize("ZANKYO_SPILL_MAX_FILES", v)?;
        }
        if let Some(v) = get("ZANKYO_SPILL_RETRY_MS") {
            cfg.spill_retry_ms = parse_u64("ZANKYO_SPILL_RETRY_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_SPILL_MAX_AGE_SECS") {
            cfg.spill_max_age_secs = parse_u64("ZANKYO_SPILL_MAX_AGE_SECS", v)?;
        }
        if let Some(v) = get("ZANKYO_MAX_BODY_KB") {
            cfg.max_body_kb = parse_usize("ZANKYO_MAX_BODY_KB", v)?;
        }
        if let Some(v) = get("ZANKYO_EXT_BODY_KB") {
            cfg.ext_body_kb = parse_usize("ZANKYO_EXT_BODY_KB", v)?;
        }
        if let Some(v) = get("ZANKYO_REGISTER_TIMEOUT_MS") {
            cfg.register_timeout_ms = parse_u64("ZANKYO_REGISTER_TIMEOUT_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_EXT_RETRY_MS") {
            cfg.ext_retry_ms = parse_u64("ZANKYO_EXT_RETRY_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_EXT_MAX_POLL_FAILURES") {
            cfg.ext_max_poll_failures = parse_u32("ZANKYO_EXT_MAX_POLL_FAILURES", v)?;
        }
        if let Some(v) = get("ZANKYO_SSM_TIMEOUT_MS") {
            cfg.ssm_timeout_ms = parse_u64("ZANKYO_SSM_TIMEOUT_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_FORWARD_TIMEOUT_MS") {
            cfg.forward_timeout_ms = parse_u64("ZANKYO_FORWARD_TIMEOUT_MS", v)?;
        }
        if let Some(v) = get("ZANKYO_DISABLED") {
            cfg.disabled = parse_bool(v);
        }
        Ok(cfg)
    }

    /// SSM Parameter の JSON で上書きする。JSON に存在したキーだけが
    /// env 由来の値を置き換える（部分上書き）。
    /// 数値キーは env 経路と同じく 0 を拒否する。0 値の上限・
    /// タイムアウトは設定ミスで関数本体まで壊すため。
    pub fn overlay_ssm_json(&mut self, json: &str) -> Result<()> {
        let p: Partial = serde_json::from_str(json).map_err(|e| {
            ZankyoError::Config(format!("ZANKYO_SSM_PARAM is not valid config JSON: {e}"))
        })?;
        for (key, zero) in [
            ("ZANKYO_MAX_EVENT_KB", p.max_event_kb == Some(0)),
            ("ZANKYO_FLUSH_BUDGET_MS", p.flush_budget_ms == Some(0)),
            ("ZANKYO_PUT_TIMEOUT_MS", p.put_timeout_ms == Some(0)),
            ("ZANKYO_SPILL_MAX_FILES", p.spill_max_files == Some(0)),
            ("ZANKYO_SPILL_RETRY_MS", p.spill_retry_ms == Some(0)),
            ("ZANKYO_SPILL_MAX_AGE_SECS", p.spill_max_age_secs == Some(0)),
            ("ZANKYO_MAX_BODY_KB", p.max_body_kb == Some(0)),
            ("ZANKYO_EXT_BODY_KB", p.ext_body_kb == Some(0)),
            (
                "ZANKYO_REGISTER_TIMEOUT_MS",
                p.register_timeout_ms == Some(0),
            ),
            ("ZANKYO_EXT_RETRY_MS", p.ext_retry_ms == Some(0)),
            (
                "ZANKYO_EXT_MAX_POLL_FAILURES",
                p.ext_max_poll_failures == Some(0),
            ),
            ("ZANKYO_SSM_TIMEOUT_MS", p.ssm_timeout_ms == Some(0)),
            ("ZANKYO_FORWARD_TIMEOUT_MS", p.forward_timeout_ms == Some(0)),
        ] {
            if zero {
                return Err(ZankyoError::Config(format!(
                    "{key} must be a positive integer"
                )));
            }
        }
        if let Some(v) = p.bucket {
            self.bucket = v;
        }
        if let Some(v) = p.kms_key {
            self.kms_key = Some(v);
        }
        if let Some(v) = p.scrub_fields {
            self.scrub_fields = parse_fields(&v);
        }
        if let Some(v) = p.scrub_mode {
            self.scrub_mode = parse_mode(&v)?;
        }
        if let Some(v) = p.max_event_kb {
            self.max_event_kb = v;
        }
        if let Some(v) = p.flush_budget_ms {
            self.flush_budget_ms = v;
        }
        if let Some(v) = p.put_timeout_ms {
            self.put_timeout_ms = v;
        }
        if let Some(v) = p.spill_dir {
            self.spill_dir = v;
        }
        if let Some(v) = p.spill_max_files {
            self.spill_max_files = v;
        }
        if let Some(v) = p.spill_retry_ms {
            self.spill_retry_ms = v;
        }
        if let Some(v) = p.spill_max_age_secs {
            self.spill_max_age_secs = v;
        }
        if let Some(v) = p.max_body_kb {
            self.max_body_kb = v;
        }
        if let Some(v) = p.ext_body_kb {
            self.ext_body_kb = v;
        }
        if let Some(v) = p.register_timeout_ms {
            self.register_timeout_ms = v;
        }
        if let Some(v) = p.ext_retry_ms {
            self.ext_retry_ms = v;
        }
        if let Some(v) = p.ext_max_poll_failures {
            self.ext_max_poll_failures = v;
        }
        if let Some(v) = p.ssm_timeout_ms {
            self.ssm_timeout_ms = v;
        }
        if let Some(v) = p.forward_timeout_ms {
            self.forward_timeout_ms = v;
        }
        if let Some(v) = p.disabled {
            self.disabled = v;
        }
        Ok(())
    }
}

/// SSM JSON のスキーマ。env 名と同じキー名を使う。
#[derive(Debug, Deserialize)]
struct Partial {
    #[serde(rename = "ZANKYO_BUCKET")]
    bucket: Option<String>,
    #[serde(rename = "ZANKYO_KMS_KEY")]
    kms_key: Option<String>,
    #[serde(rename = "ZANKYO_SCRUB_FIELDS")]
    scrub_fields: Option<String>,
    #[serde(rename = "ZANKYO_SCRUB_MODE")]
    scrub_mode: Option<String>,
    #[serde(rename = "ZANKYO_MAX_EVENT_KB")]
    max_event_kb: Option<usize>,
    #[serde(rename = "ZANKYO_FLUSH_BUDGET_MS")]
    flush_budget_ms: Option<u64>,
    #[serde(rename = "ZANKYO_PUT_TIMEOUT_MS")]
    put_timeout_ms: Option<u64>,
    #[serde(rename = "ZANKYO_SPILL_DIR")]
    spill_dir: Option<String>,
    #[serde(rename = "ZANKYO_SPILL_MAX_FILES")]
    spill_max_files: Option<usize>,
    #[serde(rename = "ZANKYO_SPILL_RETRY_MS")]
    spill_retry_ms: Option<u64>,
    #[serde(rename = "ZANKYO_SPILL_MAX_AGE_SECS")]
    spill_max_age_secs: Option<u64>,
    #[serde(rename = "ZANKYO_MAX_BODY_KB")]
    max_body_kb: Option<usize>,
    #[serde(rename = "ZANKYO_EXT_BODY_KB")]
    ext_body_kb: Option<usize>,
    #[serde(rename = "ZANKYO_REGISTER_TIMEOUT_MS")]
    register_timeout_ms: Option<u64>,
    #[serde(rename = "ZANKYO_EXT_RETRY_MS")]
    ext_retry_ms: Option<u64>,
    #[serde(rename = "ZANKYO_EXT_MAX_POLL_FAILURES")]
    ext_max_poll_failures: Option<u32>,
    #[serde(rename = "ZANKYO_SSM_TIMEOUT_MS")]
    ssm_timeout_ms: Option<u64>,
    #[serde(rename = "ZANKYO_FORWARD_TIMEOUT_MS")]
    forward_timeout_ms: Option<u64>,
    #[serde(rename = "ZANKYO_DISABLED")]
    disabled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_applied() {
        let cfg = Config::from_env_map(&env(&[("ZANKYO_BUCKET", "b")])).unwrap();
        assert_eq!(cfg.bucket, "b");
        assert_eq!(cfg.scrub_mode, ScrubMode::Mask);
        assert_eq!(cfg.max_event_kb, 256);
        assert!(!cfg.disabled);
    }

    #[test]
    fn parses_all_fields() {
        let cfg = Config::from_env_map(&env(&[
            ("ZANKYO_BUCKET", "b"),
            ("ZANKYO_KMS_KEY", "arn:aws:kms:..."),
            ("ZANKYO_SCRUB_FIELDS", "my-secret,Other_Key"),
            ("ZANKYO_SCRUB_MODE", "hash"),
            ("ZANKYO_MAX_EVENT_KB", "64"),
            ("ZANKYO_MAX_BODY_KB", "4096"),
            ("ZANKYO_EXT_MAX_POLL_FAILURES", "10"),
            ("ZANKYO_SSM_TIMEOUT_MS", "3000"),
            ("ZANKYO_DISABLED", "true"),
        ]))
        .unwrap();
        assert_eq!(cfg.max_body_kb, 4096);
        assert_eq!(cfg.ext_max_poll_failures, 10);
        assert_eq!(cfg.ssm_timeout_ms, 3000);
        assert_eq!(cfg.scrub_mode, ScrubMode::Hash);
        assert!(cfg.scrub_fields.contains("mysecret"));
        assert!(cfg.scrub_fields.contains("otherkey"));
        assert_eq!(cfg.max_event_kb, 64);
        assert!(cfg.disabled);
    }

    #[test]
    fn rejects_bad_mode() {
        assert!(Config::from_env_map(&env(&[("ZANKYO_SCRUB_MODE", "yes")])).is_err());
    }

    #[test]
    fn ssm_overrides_env_only_when_key_present() {
        let mut cfg = Config::from_env_map(&env(&[
            ("ZANKYO_BUCKET", "env-bucket"),
            ("ZANKYO_MAX_EVENT_KB", "64"),
        ]))
        .unwrap();
        cfg.overlay_ssm_json(r#"{"ZANKYO_BUCKET":"ssm-bucket"}"#)
            .unwrap();
        assert_eq!(cfg.bucket, "ssm-bucket");
        assert_eq!(cfg.max_event_kb, 64); // env 側の値が残る
    }

    #[test]
    fn ssm_rejects_invalid_json() {
        let mut cfg = Config::from_env_map(&env(&[])).unwrap();
        assert!(cfg.overlay_ssm_json("not json").is_err());
    }
}
