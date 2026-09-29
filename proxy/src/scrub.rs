//! PII scrub: フィールド名 denylist + パターン検出のハイブリッド。
//!
//! 設計上の判断:
//! - 既定 ON（`ScrubMode::Off` は明示設定のみ）で、保存されるレコードには
//!   `scrubReport` を併記し「何が消えたか」を監査可能にする。
//! - mask は `***` 一括ではなく形状保持（`j***@e***.com` 型）にする。
//!   すべて潰すと差分リプレイや sam local 再現の妨げになるため。
//! - 純粋関数として実装し、IO（S3/時刻）に触れない。テストはこのファイル内。

use crate::config::ScrubMode;
use crate::scrub_data::{DEFAULT_DENYLIST, PATTERNS};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// フィールド名の正規化: 英数字以外を落として小文字化。
/// `api-key`/`apiKey`/`API_KEY` がすべて `apikey` になる。
pub fn normalize_field(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// フィールド名を語境界で分割し、各トークンを小文字化して返す。
/// 境界 = 非英数字、小文字→大文字（camelCase）、英字↔数字の遷移。
/// 例: `pinCount`→[pin,count]、`api_key`→[api,key]、`xsrfToken`→[xsrf,token]
fn field_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in name.chars() {
        let boundary = match prev {
            Some(p) => {
                (p.is_ascii_lowercase() && c.is_ascii_uppercase())
                    || (p.is_ascii_alphabetic() && c.is_ascii_digit())
                    || (p.is_ascii_digit() && c.is_ascii_alphabetic())
            }
            None => false,
        };
        if !c.is_ascii_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev = None;
            continue;
        }
        if boundary && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        cur.push(c.to_ascii_lowercase());
        prev = Some(c);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// 連続するトークンの結合が `d` と一致するウィンドウがあるか。
/// [user, api, key] の "apikey" のような複合語 denylist 項を拾う。
fn token_window_match(tokens: &[String], d: &str) -> bool {
    for i in 0..tokens.len() {
        let mut joined = String::new();
        for t in &tokens[i..] {
            joined.push_str(t);
            if joined.len() >= d.len() {
                break;
            }
        }
        if joined == d {
            return true;
        }
    }
    false
}

struct Compiled {
    name: &'static str,
    re: Regex,
    validate: fn(&str) -> bool,
}

#[derive(Debug, Default)]
pub struct ScrubReport {
    pub fields_redacted: usize,
    pub patterns_applied: BTreeSet<String>,
}

pub struct Scrubber {
    mode: ScrubMode,
    denied: Vec<String>,
    patterns: Vec<Compiled>,
    /// hash モードの HMAC キー。関数名+バケットから導出する。
    /// 目的は暗号化ではなく擬似名化（同一値が同一ハッシュになる再現性）なので、
    /// 秘密値ではなく環境から決定的に作る。
    hash_key: [u8; 32],
}

impl Scrubber {
    pub fn new(mode: ScrubMode, extra_fields: &BTreeSet<String>, key_seed: &str) -> Self {
        let mut denied: Vec<String> = DEFAULT_DENYLIST.iter().map(|s| s.to_string()).collect();
        denied.extend(extra_fields.iter().cloned());
        let patterns = PATTERNS
            .iter()
            .map(|p| Compiled {
                name: p.name,
                re: Regex::new(p.re).expect("builtin pattern must compile"),
                validate: p.validate,
            })
            .collect();
        let hash_key: [u8; 32] = Sha256::digest(key_seed.as_bytes()).into();
        Scrubber {
            mode,
            denied,
            patterns,
            hash_key,
        }
    }

    /// denylist 判定。名全体の正規化一致（`api_key`→apikey、
    /// `pass_word`→password 等）か、連続トークン結合の一致
    /// （`userApiKey`→[user,api,key]→apikey、`pinCount`→pin）で判定する。
    /// トークン内部の部分文字列（`spin`⊃pin、`tokenize`⊃token、
    /// `secretary`⊃secret）は対象にしない — 秘密でない値のマスクは
    /// fixture/replay の再現データを壊す。
    fn is_denied(&self, field: &str) -> bool {
        let n = normalize_field(field);
        let tokens = field_tokens(field);
        self.denied
            .iter()
            .any(|d| n == *d || token_window_match(&tokens, d))
    }

    /// JSON 値を in-place で scrub する。
    pub fn scrub(&self, v: &mut Value, report: &mut ScrubReport) {
        if self.mode == ScrubMode::Off {
            return;
        }
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if self.is_denied(k) {
                        *val = self.redact_node(val);
                        report.fields_redacted += 1;
                    } else {
                        self.scrub(val, report);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.scrub(item, report);
                }
            }
            Value::String(s) => {
                let next = self
                    .scrub_embedded_json(s, report)
                    .unwrap_or_else(|| self.scrub_string(s, report));
                *s = next;
            }
            _ => {}
        }
    }

    /// 文字列化された JSON（API Gateway・Function URL の `body` 等）の
    /// フィールドにも denylist を効かせる。denylist に一致した場合だけ
    /// 再シリアライズした文字列を返す。一致しなければ None を返し、
    /// 原文へのパターン置換に任せる（空白やキー順を不要に変えない）。
    fn scrub_embedded_json(&self, s: &str, report: &mut ScrubReport) -> Option<String> {
        if !matches!(s.trim_start().as_bytes().first(), Some(b'{' | b'[')) {
            return None;
        }
        let mut inner: Value = serde_json::from_str(s).ok()?;
        let mut sub = ScrubReport::default();
        self.scrub(&mut inner, &mut sub);
        if sub.fields_redacted == 0 {
            return None;
        }
        report.fields_redacted += sub.fields_redacted;
        report.patterns_applied.extend(sub.patterns_applied);
        Some(inner.to_string())
    }

    /// denylist に一致したフィールド値の置き換え。配列は要素ごとに
    /// 置き換えて形を保つ（`cookies` の文字列配列が文字列に変わると、
    /// replay でハンドラが元と別の例外で落ちるため）。
    fn redact_node(&self, v: &Value) -> Value {
        if let Value::Array(items) = v {
            return Value::Array(items.iter().map(|i| self.redact_node(i)).collect());
        }
        match self.mode {
            ScrubMode::Off => v.clone(),
            ScrubMode::Hash => Value::String(format!("hmac:{}", self.hmac(v))),
            ScrubMode::Mask => Value::String(match v {
                Value::String(s) => mask_shape(s),
                _ => "***".to_string(),
            }),
        }
    }

    fn hmac(&self, v: &Value) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.hash_key)
            .expect("HMAC accepts any key length");
        match v {
            Value::String(s) => mac.update(s.as_bytes()),
            other => mac.update(other.to_string().as_bytes()),
        }
        let out = mac.finalize().into_bytes();
        hex::encode(&out[..8])
    }

    /// 自由テキスト（errorMessage 等）に対するパターン置換。
    /// denylist 照合はフィールド名を持たないテキストには適用しない。
    /// `ScrubMode::Off` では `scrub` と同じく何も置き換えない。
    pub fn scrub_text(&self, s: &str, report: &mut ScrubReport) -> String {
        if self.mode == ScrubMode::Off {
            return s.to_string();
        }
        self.scrub_string(s, report)
    }

    /// 文字列値の中に埋まったパターンを置き換える。
    fn scrub_string(&self, s: &str, report: &mut ScrubReport) -> String {
        let mut out = s.to_string();
        for p in &self.patterns {
            if !p.re.is_match(&out) {
                continue;
            }
            let mut applied = false;
            let next =
                p.re.replace_all(&out, |caps: &regex::Captures| {
                    let m = &caps[0];
                    if !(p.validate)(m) {
                        return m.to_string();
                    }
                    applied = true;
                    self.mask_for(p.name, m)
                })
                .into_owned();
            out = next;
            if applied {
                report.patterns_applied.insert(p.name.to_string());
            }
        }
        out
    }

    fn mask_for(&self, name: &str, m: &str) -> String {
        if self.mode == ScrubMode::Hash {
            return format!("hmac:{}", self.hmac(&Value::String(m.to_string())));
        }
        match name {
            "email" => mask_email(m),
            "jwt" => "eyJ***.***.***".to_string(),
            "credit_card" | "aws_access_key" | "phone" => mask_keep_last(m, 4),
            "bearer_token" => mask_bearer(m),
            "ipv4" => mask_ipv4(m),
            _ => mask_shape(m),
        }
    }
}

fn mask_shape(s: &str) -> String {
    match s.chars().next() {
        Some(c) if s.chars().count() > 4 => format!("{c}***"),
        _ => "***".to_string(),
    }
}

fn mask_email(s: &str) -> String {
    let Some((local, domain)) = s.split_once('@') else {
        return mask_shape(s);
    };
    let (dname, tld) = domain.rsplit_once('.').unwrap_or((domain, ""));
    let l0 = local.chars().next().unwrap_or('*');
    let d0 = dname.chars().next().unwrap_or('*');
    format!("{l0}***@{d0}***.{tld}")
}

fn mask_keep_last(s: &str, n: usize) -> String {
    let tail: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .chars()
        .rev()
        .take(n)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("***{tail}")
}

fn mask_bearer(s: &str) -> String {
    // "Bearer xxx" → スキーム部分だけ残す
    let scheme = s.split_whitespace().next().unwrap_or("Bearer");
    format!("{scheme} ***")
}

fn mask_ipv4(s: &str) -> String {
    match s.rsplit_once('.') {
        Some((_, last)) => format!("x.x.x.{last}"),
        None => "***".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scrubber() -> Scrubber {
        Scrubber::new(ScrubMode::Mask, &BTreeSet::new(), "seed")
    }

    #[test]
    fn denylist_masks_named_fields() {
        let mut v = json!({"password": "hunter2", "api-key": "abcdef", "nested": {"SessionID": "xyz123"}, "keep": "ok"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["password"], "h***");
        assert_eq!(v["nested"]["SessionID"], "x***");
        assert_eq!(v["keep"], "ok");
        assert_eq!(r.fields_redacted, 3);
    }

    #[test]
    fn denylist_matches_suffix_and_prefix() {
        let mut v = json!({"access_token": "t", "tokenExpiry": "t", "unrelated": "u"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["access_token"], "***");
        assert_eq!(v["tokenExpiry"], "***");
        assert_eq!(v["unrelated"], "u");
    }

    #[test]
    fn denylist_ignores_in_token_substrings() {
        // 語境界の内側にある部分文字列は秘密とは限らない。
        // spin/pine (pin), tokenize (token), secretary (secret) は
        // マスクしない — 再現用の正常値を壊さないため。
        let mut v = json!({
            "spin": 3, "pine": "tree", "spinner": "css",
            "tokenize": true, "tokenizer": "bert",
            "secretary": "general", "pinCount": 4,
        });
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["spin"], 3);
        assert_eq!(v["pine"], "tree");
        assert_eq!(v["spinner"], "css");
        assert_eq!(v["tokenize"], true);
        assert_eq!(v["tokenizer"], "bert");
        assert_eq!(v["secretary"], "general");
        // 語境界を跨ぐものは拾う: pinCount → [pin,count]
        assert_eq!(v["pinCount"], "***");
        assert_eq!(r.fields_redacted, 1);
    }

    #[test]
    fn denylist_matches_compound_names() {
        let mut v = json!({"userApiKey": "k", "session_id": "s", "pin_number": "9"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(r.fields_redacted, 3);
    }

    #[test]
    fn email_is_shape_preserved() {
        let mut v = json!({"to": "alice@example.com"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["to"], "a***@e***.com");
        assert!(r.patterns_applied.contains("email"));
    }

    #[test]
    fn jwt_is_masked() {
        let mut v = json!({"auth": "id eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c end"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert!(v["auth"].as_str().unwrap().contains("eyJ***.***.***"));
    }

    #[test]
    fn card_requires_luhn() {
        // Luhn 成立: 4111 1111 1111 1111 / 不成立: 4111 1111 1111 1112
        let mut v = json!({"a": "4111 1111 1111 1111", "b": "4111 1111 1111 1112"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["a"], "***1111");
        assert_eq!(v["b"], "4111 1111 1111 1112");
    }

    #[test]
    fn aws_access_key_keeps_last_four() {
        let mut v = json!({"k": "AKIAIOSFODNN7EXAMPLE"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["k"], "***MPLE");
        assert!(r.patterns_applied.contains("aws_access_key"));
    }

    #[test]
    fn bearer_token_keeps_only_scheme() {
        let mut v = json!({"note": "Authorization: Bearer abcdefghijk123"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["note"], "Authorization: Bearer ***");
        assert!(r.patterns_applied.contains("bearer_token"));
    }

    #[test]
    fn phone_keeps_last_four() {
        let mut v = json!({"a": "call +81-90-1234-5678", "b": "555-123-4567"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["a"], "call ***5678");
        assert_eq!(v["b"], "***4567");
        assert!(r.patterns_applied.contains("phone"));
    }

    #[test]
    fn ipv4_keeps_last_octet() {
        let mut v = json!({"src": "from 192.168.10.25", "ver": "999.1.1.1"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["src"], "from x.x.x.25");
        assert_eq!(v["ver"], "999.1.1.1");
        assert!(r.patterns_applied.contains("ipv4"));
    }

    #[test]
    fn cookies_are_denied_and_keep_array_shape() {
        let mut v = json!({
            "headers": {"Cookie": "sid=abc123"},
            "cookies": ["sid=abc123", "theme=dark"],
            "multiValueHeaders": {"Set-Cookie": ["sid=abc123; HttpOnly"]},
        });
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["headers"]["Cookie"], "s***");
        assert_eq!(v["cookies"], json!(["s***", "t***"]));
        assert_eq!(v["multiValueHeaders"]["Set-Cookie"], json!(["s***"]));
        assert_eq!(r.fields_redacted, 3);
    }

    #[test]
    fn stringified_json_body_is_walked() {
        // API Gateway プロキシ統合の body は JSON を文字列化した値
        let mut v = json!({
            "body": r#"{"username":"alice","password":"hunter2"}"#,
            "isBase64Encoded": false,
        });
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        let body: Value = serde_json::from_str(v["body"].as_str().unwrap()).unwrap();
        assert_eq!(body, json!({"username": "alice", "password": "h***"}));
        assert_eq!(r.fields_redacted, 1);
    }

    #[test]
    fn stringified_json_without_denied_fields_keeps_original_text() {
        let raw = r#"{ "to": "alice@example.com",  "n": 1 }"#;
        let mut v = json!({"body": raw, "log": "[INFO] password reset"});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(v["body"], r#"{ "to": "a***@e***.com",  "n": 1 }"#);
        assert_eq!(v["log"], "[INFO] password reset");
        assert_eq!(r.fields_redacted, 0);
    }

    #[test]
    fn arrays_and_nested_objects_are_walked() {
        let mut v = json!({"items": [{"password": "x"}, {"password": "y"}]});
        let mut r = ScrubReport::default();
        scrubber().scrub(&mut v, &mut r);
        assert_eq!(r.fields_redacted, 2);
    }

    #[test]
    fn hash_mode_is_deterministic() {
        let s = Scrubber::new(ScrubMode::Hash, &BTreeSet::new(), "seed");
        let mut a = json!({"password": "same"});
        let mut b = json!({"password": "same"});
        let mut r = ScrubReport::default();
        s.scrub(&mut a, &mut r);
        s.scrub(&mut b, &mut r);
        assert_eq!(a, b);
        assert!(a["password"].as_str().unwrap().starts_with("hmac:"));
    }

    #[test]
    fn off_mode_leaves_values() {
        let s = Scrubber::new(ScrubMode::Off, &BTreeSet::new(), "seed");
        let mut v = json!({"password": "plain"});
        let mut r = ScrubReport::default();
        s.scrub(&mut v, &mut r);
        assert_eq!(v["password"], "plain");
        assert_eq!(r.fields_redacted, 0);
    }

    #[test]
    fn off_mode_leaves_free_text() {
        // off は errorMessage 等の自由テキストにもパターン置換を掛けない
        let s = Scrubber::new(ScrubMode::Off, &BTreeSet::new(), "seed");
        let text = "alice@example.com AKIAIOSFODNN7EXAMPLE Authorization: Bearer abcdefghijk123";
        let mut r = ScrubReport::default();
        assert_eq!(s.scrub_text(text, &mut r), text);
        assert!(r.patterns_applied.is_empty());
    }

    #[test]
    fn extra_fields_extend_denylist() {
        let extra: BTreeSet<String> = ["mysecret".to_string()].into_iter().collect();
        let s = Scrubber::new(ScrubMode::Mask, &extra, "seed");
        let mut v = json!({"my-secret": "x"});
        let mut r = ScrubReport::default();
        s.scrub(&mut v, &mut r);
        assert_eq!(r.fields_redacted, 1);
    }
}
