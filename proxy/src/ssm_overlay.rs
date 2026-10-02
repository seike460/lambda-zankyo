//! `ZANKYO_SSM_PARAM` 由来の JSON overlay。
//!
//! 設定全体の解決順序と既定値は `config.rs` 参照。ここでは
//! SSM 値の型別変換とキー → フィールドの写像だけを扱う。
//! 数値キーは `SSM_NUM_FIELDS` テーブル駆動なので、新しい数値設定は
//! 1 行足すだけで overlay 対応になる。ただし SSM を読む前に使う値は
//! 載せない（`ENV_ONLY_KEYS`）。

use crate::config::{parse_bool, parse_fields, parse_mode, valid_spill_dir, Config};
use crate::error::{Result, ZankyoError};

/// Config の数値フィールドへ値を書き込む setter。
type NumSetter = fn(&mut Config, u64);

/// 数値キー → フィールドの写像テーブル。新しい数値設定はここに
/// 1 行足すだけでよい。値は `ssm_u64` で正整数のみ受理される
/// （0・型違い・負数はそのキーだけ警告して無視）。
const SSM_NUM_FIELDS: &[(&str, NumSetter)] = &[
    ("ZANKYO_MAX_EVENT_KB", |c, n| c.max_event_kb = n as usize),
    ("ZANKYO_FLUSH_BUDGET_MS", |c, n| c.flush_budget_ms = n),
    ("ZANKYO_PUT_TIMEOUT_MS", |c, n| c.put_timeout_ms = n),
    ("ZANKYO_SPILL_MAX_FILES", |c, n| {
        c.spill_max_files = n as usize
    }),
    ("ZANKYO_SPILL_RETRY_MS", |c, n| c.spill_retry_ms = n),
    ("ZANKYO_SPILL_MAX_AGE_SECS", |c, n| c.spill_max_age_secs = n),
    ("ZANKYO_MAX_BODY_KB", |c, n| c.max_body_kb = n as usize),
    ("ZANKYO_EXT_BODY_KB", |c, n| c.ext_body_kb = n as usize),
    ("ZANKYO_REGISTER_TIMEOUT_MS", |c, n| {
        c.register_timeout_ms = n
    }),
    ("ZANKYO_EXT_RETRY_MS", |c, n| c.ext_retry_ms = n),
    ("ZANKYO_FORWARD_TIMEOUT_MS", |c, n| c.forward_timeout_ms = n),
];

/// env でだけ指定できるキー。どちらも SSM を読む前に使う（取得先の
/// パラメータ名と取得の上限時間）ため、JSON で上書きしても効かない。
/// 書かれていたら、未知のキーとは別の警告を出して無視する。
const ENV_ONLY_KEYS: &[&str] = &["ZANKYO_SSM_PARAM", "ZANKYO_SSM_TIMEOUT_MS"];

fn num_setter(key: &str) -> Option<NumSetter> {
    SSM_NUM_FIELDS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, f)| *f)
}

impl Config {
    /// SSM Parameter の JSON で上書きする。JSON に存在したキーだけが
    /// env 由来の値を置き換える（部分上書き）。
    /// パースはキーごとに行い、型が合わない値はそのキーだけ警告して
    /// 飛ばす — 1 フィールドの書き損じで設定全体（bucket 含む）を
    /// 失い、記録が無断停止する事態を避ける。
    /// 数値・真偽値は JSON ネイティブ型に加えて文字列形式も受理する
    /// （env 経路との対称性）。0 値は env と同じく拒否する
    /// （0 の上限・タイムアウトは設定ミスで関数本体まで壊す）。
    pub fn overlay_ssm_json(&mut self, json: &str) -> Result<()> {
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| {
            ZankyoError::Config(format!("ZANKYO_SSM_PARAM is not valid config JSON: {e}"))
        })?;
        let Some(obj) = v.as_object() else {
            return Err(ZankyoError::Config(
                "ZANKYO_SSM_PARAM must be a JSON object".into(),
            ));
        };
        for (key, val) in obj {
            if let Some(setter) = num_setter(key) {
                if let Some(n) = ssm_u64(key, val) {
                    setter(self, n);
                }
                continue;
            }
            match key.as_str() {
                "ZANKYO_BUCKET" => {
                    // "" で env 側のバケットを消さない
                    if let Some(s) = ssm_nonempty(key, val) {
                        self.bucket = s;
                    }
                }
                "ZANKYO_KMS_KEY" => {
                    if let Some(s) = ssm_nonempty(key, val) {
                        self.kms_key = Some(s);
                    }
                }
                "ZANKYO_SCRUB_FIELDS" => {
                    if let Some(s) = ssm_str(key, val) {
                        self.scrub_fields = parse_fields(&s);
                    }
                }
                "ZANKYO_SCRUB_MODE" => {
                    if let Some(s) = ssm_str(key, val) {
                        match parse_mode(&s) {
                            Ok(m) => self.scrub_mode = m,
                            Err(e) => {
                                tracing::warn!(error = %e, key, "invalid SSM config value; ignored")
                            }
                        }
                    }
                }
                "ZANKYO_DISABLED" => {
                    if let Some(b) = ssm_bool(key, val) {
                        self.disabled = b;
                    }
                }
                "ZANKYO_SPILL_DIR" => {
                    if let Some(s) = ssm_str(key, val) {
                        if let Some(d) = valid_spill_dir(&s) {
                            self.spill_dir = d;
                        }
                    }
                }
                "ZANKYO_EXT_MAX_POLL_FAILURES" => {
                    // u64 → u32 の変換は明示失敗にする。`as` だと
                    // u32::MAX 超が wrap して意図しない小値になる。
                    if let Some(n) = ssm_u64(key, val) {
                        match u32::try_from(n) {
                            Ok(n) => self.ext_max_poll_failures = n,
                            Err(_) => {
                                tracing::warn!(key, "SSM config value exceeds u32 range; ignored")
                            }
                        }
                    }
                }
                k if ENV_ONLY_KEYS.contains(&k) => tracing::warn!(
                    key = k,
                    "this key is used before the SSM fetch and can only be set in the environment; ignored"
                ),
                other => tracing::warn!(key = other, "unknown ZANKYO_SSM_PARAM key; ignored"),
            }
        }
        Ok(())
    }
}

/// SSM 値から文字列を取る。型違いは警告して None。
fn ssm_str(key: &str, v: &serde_json::Value) -> Option<String> {
    match v.as_str() {
        Some(s) => Some(s.to_string()),
        None => {
            tracing::warn!(key, "SSM config value is not a string; ignored");
            None
        }
    }
}

/// 空でない文字列値を取る。"ZANKYO_BUCKET":"" のような上書きで
/// env 側の値を消さないよう、空文字は警告して無視する。
fn ssm_nonempty(key: &str, v: &serde_json::Value) -> Option<String> {
    let s = ssm_str(key, v)?;
    if s.is_empty() {
        tracing::warn!(key, "empty SSM config value ignored");
        return None;
    }
    Some(s)
}

/// SSM 値から真偽値を取る。bool のほか env と同じ文字列形式も受理。
fn ssm_bool(key: &str, v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => Some(parse_bool(s)),
        _ => {
            tracing::warn!(key, "SSM config value is not a boolean; ignored");
            None
        }
    }
}

/// SSM 値から正整数を取る。JSON 数値のほか数値文字列も受理。
/// 0・非数値・負数は警告して None（そのキーだけ無効）。
fn ssm_u64(key: &str, v: &serde_json::Value) -> Option<u64> {
    let n = match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    };
    match n {
        Some(n) if n > 0 => Some(n),
        _ => {
            tracing::warn!(key, "SSM config value is not a positive integer; ignored");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_FLUSH_BUDGET_MS, DEFAULT_PUT_TIMEOUT_MS};
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
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
        assert!(cfg.overlay_ssm_json("[1,2]").is_err());
    }

    #[test]
    fn ssm_bad_field_does_not_drop_whole_overlay() {
        // 1 フィールドの型違いで overlay 全体が捨てられると、
        // SSM 側にしか無い bucket まで失って記録が止まる。
        let mut cfg = Config::from_env_map(&env(&[])).unwrap();
        cfg.overlay_ssm_json(
            r#"{"ZANKYO_BUCKET":"b","ZANKYO_DISABLED":"true","ZANKYO_MAX_EVENT_KB":"64","ZANKYO_PUT_TIMEOUT_MS":"oops","ZANKYO_FLUSH_BUDGET_MS":0}"#,
        )
        .unwrap();
        assert_eq!(cfg.bucket, "b");
        assert!(cfg.disabled); // 文字列 "true" も受理
        assert_eq!(cfg.max_event_kb, 64); // 文字列数値も受理
        assert_eq!(cfg.put_timeout_ms, DEFAULT_PUT_TIMEOUT_MS); // 型違いは既定値のまま
        assert_eq!(cfg.flush_budget_ms, DEFAULT_FLUSH_BUDGET_MS); // 0 は拒否
    }

    #[test]
    fn ssm_does_not_overlay_keys_used_before_the_fetch() {
        // SSM の取得先と取得の上限時間は、取得の前に env から決まる。
        // JSON の値で Config を書き換えると、効かない値が設定に見えてしまう。
        let mut cfg = Config::from_env_map(&env(&[
            ("ZANKYO_SSM_PARAM", "/zankyo/config"),
            ("ZANKYO_SSM_TIMEOUT_MS", "3000"),
        ]))
        .unwrap();
        cfg.overlay_ssm_json(
            r#"{"ZANKYO_SSM_PARAM":"/other","ZANKYO_SSM_TIMEOUT_MS":9000,"ZANKYO_BUCKET":"b"}"#,
        )
        .unwrap();
        assert_eq!(cfg.ssm_timeout_ms, 3000);
        assert_eq!(cfg.ssm_param.as_deref(), Some("/zankyo/config"));
        assert_eq!(cfg.bucket, "b"); // ほかのキーは従来どおり上書きする
        for key in ENV_ONLY_KEYS {
            assert!(
                num_setter(key).is_none(),
                "{key} must not be in SSM_NUM_FIELDS"
            );
        }
    }

    #[test]
    fn ssm_ext_max_poll_failures_rejects_u32_overflow() {
        // `as` キャストだと u32::MAX 超が wrap する。明示失敗で既定値を守る。
        let mut cfg = Config::from_env_map(&env(&[])).unwrap();
        cfg.overlay_ssm_json(r#"{"ZANKYO_EXT_MAX_POLL_FAILURES":5000000000}"#)
            .unwrap();
        assert_eq!(
            cfg.ext_max_poll_failures,
            crate::config::DEFAULT_EXT_MAX_POLL_FAILURES
        );
        cfg.overlay_ssm_json(r#"{"ZANKYO_EXT_MAX_POLL_FAILURES":42}"#)
            .unwrap();
        assert_eq!(cfg.ext_max_poll_failures, 42);
    }
}
