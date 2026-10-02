//! 設定の解決。
//!
//! 既定値 → 環境変数 → SSM の順に重ね、後から重ねた値が優先する。
//! `ZANKYO_SSM_PARAM` が指定されている場合は SSM Parameter Store の JSON を
//! 読み、同じキーがあれば env を上書きする（複数関数で設定を一元管理したい
//! 利用者のため）。SSM 側のスキーマは env 名と同じキーを持つフラットな
//! JSON オブジェクト。ただし次は SSM を読む前に決まる（`setup.rs`）。
//! - `ZANKYO_SSM_PARAM`・`ZANKYO_SSM_TIMEOUT_MS` は env だけで決める
//!   （`ssm_overlay.rs` の `ENV_ONLY_KEYS`）。
//! - env の `ZANKYO_DISABLED` が真なら、SSM を読まずに passthrough にする。
//! - env の値が不正なら設定エラーとし、SSM を読まずに passthrough にする。

use crate::error::{Result, ZankyoError};
use std::collections::{BTreeSet, HashMap};

pub const DEFAULT_MAX_EVENT_KB: usize = 256;
pub const DEFAULT_FLUSH_BUDGET_MS: u64 = 1_200;
pub const DEFAULT_PUT_TIMEOUT_MS: u64 = 5_000;
/// spill dir の既定プレフィックス。`AWS_LAMBDA_FUNCTION_NAME` がある
/// 環境では `/tmp/zankyo/<function>` と関数名でスコープする。
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
/// extension 登録の上限時間（ms）。一時的な失敗の再試行もこの時間内に収める。
pub const DEFAULT_REGISTER_TIMEOUT_MS: u64 = 10_000;
/// Extensions API（register・event/next）が失敗したときの再試行間隔（ms）。
pub const DEFAULT_EXT_RETRY_MS: u64 = 500;
/// Extensions API に接続できない状態が続いたときの再試行回数の上限。
/// 既定では 500ms × 120 ≒ 60 秒接続できなければ、Runtime API が無くなった
/// とみなしてループを抜ける。応答が返る失敗（500 以外）はこの回数に数えず、
/// SHUTDOWN まで再試行を続ける。
pub const DEFAULT_EXT_MAX_POLL_FAILURES: u32 = 120;
/// SSM get_parameter の上限時間（ms）。取得は子の起動と extension 登録の
/// 前に待つため、Lambda の Init 上限（10 秒）を使い切らない短めの既定にする。
/// 取得の前に使う値なので、env でだけ指定できる（SSM の JSON では変えられない）。
pub const DEFAULT_SSM_TIMEOUT_MS: u64 = 2_000;
/// 上流転送の上限時間（ms）。localhost 上の Runtime API が
/// 60 秒応えない状況は実行環境の異常とみなす。`/next` はロングポーリングなので
/// 応答ヘッダーまでは無制限に待ち、その後のボディの読み取りにだけ使う。
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
    /// 失敗レコードの PutObject に使える時間の上限。呼び出し中の記録
    /// （応答の転送をこの時間まで待たせる）と SHUTDOWN 後のフラッシュに効く。
    pub flush_budget_ms: u64,
    /// spill 再送（起動時・定期回収）の PutObject 上限時間。
    pub put_timeout_ms: u64,
    /// `.inflight` ステージと、PUT 前に先書きする失敗レコード（write-ahead spill）の置き場。
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
    /// extension 登録の上限時間（ms）。再試行を含む。
    pub register_timeout_ms: u64,
    /// Extensions API（register・event/next）が失敗したときの再試行間隔（ms）。
    pub ext_retry_ms: u64,
    /// Extensions API に接続できない状態が続いたときの再試行回数の上限。
    /// 超えると extension ループを抜ける。
    pub ext_max_poll_failures: u32,
    /// SSM get_parameter の上限時間（ms）。env でだけ指定できる。
    pub ssm_timeout_ms: u64,
    /// 上流転送の上限時間（ms）。`/next` では応答ボディの読み取りにだけ使う。
    pub forward_timeout_ms: u64,
    pub disabled: bool,
}

pub(crate) fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

pub(crate) fn parse_mode(v: &str) -> Result<ScrubMode> {
    match v.to_ascii_lowercase().as_str() {
        "mask" => Ok(ScrubMode::Mask),
        "hash" => Ok(ScrubMode::Hash),
        "off" => Ok(ScrubMode::Off),
        other => Err(ZankyoError::Config(format!(
            "invalid ZANKYO_SCRUB_MODE {other:?} (expected mask|hash|off)"
        ))),
    }
}

pub(crate) fn parse_fields(v: &str) -> BTreeSet<String> {
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
            // 空文字は「未設定」と同じ扱いにする。
            // Some("") の kms_key は全 PutObject を失敗させ、
            // Some("") の ssm_param は init のたびに空名で SSM を呼び、
            // "" の bucket は SSM 側の値を後から潰すだけになる。
            bucket: get("ZANKYO_BUCKET").unwrap_or_default().to_string(),
            kms_key: get("ZANKYO_KMS_KEY")
                .filter(|s| !s.is_empty())
                .map(String::from),
            ssm_param: get("ZANKYO_SSM_PARAM")
                .filter(|s| !s.is_empty())
                .map(String::from),
            scrub_fields: BTreeSet::new(),
            scrub_mode: ScrubMode::Mask,
            max_event_kb: DEFAULT_MAX_EVENT_KB,
            flush_budget_ms: DEFAULT_FLUSH_BUDGET_MS,
            put_timeout_ms: DEFAULT_PUT_TIMEOUT_MS,
            spill_dir: default_spill_dir(env),
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
            if let Some(d) = valid_spill_dir(v) {
                cfg.spill_dir = d;
            }
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
}

/// 既定の spill dir。関数名でスコープすることで、同一 sandbox を共有する
/// 他関数の残滓と混ざらず、回収も関数単位で完結する。
/// 関数名はパスに使える文字だけへ正規化する。
fn default_spill_dir(env: &HashMap<String, String>) -> String {
    let name = env.get("AWS_LAMBDA_FUNCTION_NAME").map(String::as_str);
    match name {
        Some(n) if !n.is_empty() => {
            let clean: String = n
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            format!("{DEFAULT_SPILL_DIR}/{clean}")
        }
        _ => DEFAULT_SPILL_DIR.to_string(),
    }
}

/// spill ディレクトリ値の検証。空文字と相対パスを弾く。
/// Lambda の CWD（/var/task）は read-only で、相対パスは
/// create_dir_all が必ず失敗して spill が毎回 warn で落ちるため、
/// 絶対パスだけを受理する。
pub(crate) fn valid_spill_dir(v: &str) -> Option<String> {
    if v.is_empty() || !std::path::Path::new(v).is_absolute() {
        tracing::warn!(
            value = v,
            "ZANKYO_SPILL_DIR must be an absolute path; ignored"
        );
        return None;
    }
    Some(v.to_string())
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
        // SSM 取得は Init の 10 秒上限の内側で諦める
        assert_eq!(cfg.ssm_timeout_ms, 2_000);
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
        assert_eq!(cfg.kms_key.as_deref(), Some("arn:aws:kms:..."));
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
    fn empty_strings_are_treated_as_unset() {
        let cfg = Config::from_env_map(&env(&[
            ("ZANKYO_BUCKET", ""),
            ("ZANKYO_KMS_KEY", ""),
            ("ZANKYO_SSM_PARAM", ""),
        ]))
        .unwrap();
        assert!(cfg.bucket.is_empty());
        assert_eq!(cfg.kms_key, None);
        assert_eq!(cfg.ssm_param, None);
    }

    #[test]
    fn rejects_zero_limits() {
        for key in [
            "ZANKYO_MAX_BODY_KB",
            "ZANKYO_FLUSH_BUDGET_MS",
            "ZANKYO_EXT_MAX_POLL_FAILURES",
        ] {
            assert!(
                Config::from_env_map(&env(&[(key, "0")])).is_err(),
                "{key}=0 must be rejected"
            );
        }
    }

    #[test]
    fn non_absolute_spill_dir_is_ignored() {
        for v in ["rel", ""] {
            let cfg = Config::from_env_map(&env(&[
                ("AWS_LAMBDA_FUNCTION_NAME", "my-fn"),
                ("ZANKYO_SPILL_DIR", v),
            ]))
            .unwrap();
            assert_eq!(cfg.spill_dir, "/tmp/zankyo/my-fn", "{v:?} must be ignored");
        }
    }

    #[test]
    fn spill_dir_defaults_to_function_scope() {
        // Lambda 環境では関数名でスコープされる
        let cfg = Config::from_env_map(&env(&[("AWS_LAMBDA_FUNCTION_NAME", "my-fn")])).unwrap();
        assert_eq!(cfg.spill_dir, "/tmp/zankyo/my-fn");
        // 関数名が無い環境（ローカル）ではプレフィックス直下
        let cfg = Config::from_env_map(&env(&[])).unwrap();
        assert_eq!(cfg.spill_dir, "/tmp/zankyo");
        // 明示指定が優先される
        let cfg = Config::from_env_map(&env(&[
            ("AWS_LAMBDA_FUNCTION_NAME", "my-fn"),
            ("ZANKYO_SPILL_DIR", "/tmp/custom"),
        ]))
        .unwrap();
        assert_eq!(cfg.spill_dir, "/tmp/custom");
    }
}
