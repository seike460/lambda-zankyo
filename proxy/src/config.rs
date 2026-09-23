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

fn parse_usize(key: &str, v: &str) -> Result<usize> {
    v.parse::<usize>()
        .map_err(|_| ZankyoError::Config(format!("{key} must be a positive integer, got {v:?}")))
}

fn parse_u64(key: &str, v: &str) -> Result<u64> {
    v.parse::<u64>()
        .map_err(|_| ZankyoError::Config(format!("{key} must be a positive integer, got {v:?}")))
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
        if let Some(v) = get("ZANKYO_DISABLED") {
            cfg.disabled = parse_bool(v);
        }
        Ok(cfg)
    }

    /// SSM Parameter の JSON で上書きする。JSON に存在したキーだけが
    /// env 由来の値を置き換える（部分上書き）。
    pub fn overlay_ssm_json(&mut self, json: &str) -> Result<()> {
        let p: Partial = serde_json::from_str(json).map_err(|e| {
            ZankyoError::Config(format!("ZANKYO_SSM_PARAM is not valid config JSON: {e}"))
        })?;
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
            ("ZANKYO_DISABLED", "true"),
        ]))
        .unwrap();
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
