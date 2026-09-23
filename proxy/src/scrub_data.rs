//! scrub の判定データ。ルールはコードではなくデータとして宣言し、
//! 追加・削除が「テーブルの 1 エントリ」で済むようにする。
//! 判定ロジック本体は scrub.rs に置く。

/// 既定のフィールド名 denylist（正規化後の形）。
/// セパレータ（`_` `-` `.` 空白）と大文字小文字は正規化で潰して照合する。
pub(crate) const DEFAULT_DENYLIST: &[&str] = &[
    "password",
    "secret",
    "token",
    "apikey",
    "authorization",
    "privatekey",
    "sessionid",
    "ssn",
    "creditcard",
    "cvv",
    "pin",
];

/// 値の内容から検出するパターン 1 件分の宣言。
pub(crate) struct PatternRule {
    pub name: &'static str,
    pub re: &'static str,
    /// 誤検出を減らすための後検証（Luhn・桁数・オクテット範囲）。
    pub validate: fn(&str) -> bool,
}

/// 検出パターンのテーブル。新しい検出器の追加はここへの 1 エントリで完結する。
pub(crate) const PATTERNS: &[PatternRule] = &[
    PatternRule {
        name: "email",
        re: r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
        validate: always,
    },
    PatternRule {
        name: "jwt",
        // JWT は header が必ず "eyJ" (base64 of `{"`) で始まる
        re: r"eyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}",
        validate: always,
    },
    PatternRule {
        name: "credit_card",
        re: r"(?:\d[ -]?){12,18}\d",
        validate: luhn_ok,
    },
    PatternRule {
        name: "aws_access_key",
        re: r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
        validate: always,
    },
    PatternRule {
        name: "bearer_token",
        re: r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{10,}",
        validate: always,
    },
    PatternRule {
        name: "phone",
        re: r"\+\d[\d .()-]{7,17}\d|\b\d{3}[-.]\d{3}[-.]\d{4}\b",
        validate: phone_ok,
    },
    PatternRule {
        name: "ipv4",
        re: r"\b\d{1,3}(?:\.\d{1,3}){3}\b",
        validate: ipv4_ok,
    },
];

fn luhn_ok(text: &str) -> bool {
    let digits: Vec<u32> = text
        .chars()
        .filter(|c| c.is_ascii_digit())
        .filter_map(|c| c.to_digit(10))
        .collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for d in digits.iter().rev() {
        let mut x = *d;
        if double {
            x *= 2;
            if x > 9 {
                x -= 9;
            }
        }
        sum += x;
        double = !double;
    }
    sum % 10 == 0
}

fn ipv4_ok(text: &str) -> bool {
    text.split('.').all(|o| o.parse::<u8>().is_ok())
}

fn phone_ok(text: &str) -> bool {
    let n = text.chars().filter(|c| c.is_ascii_digit()).count();
    (9..=15).contains(&n)
}

fn always(_: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_valid_and_rejects_invalid() {
        assert!(luhn_ok("4111 1111 1111 1111"));
        assert!(!luhn_ok("4111 1111 1111 1112"));
        assert!(!luhn_ok("12345")); // 短すぎ
    }

    #[test]
    fn ipv4_rejects_out_of_range_octets() {
        assert!(ipv4_ok("10.0.0.1"));
        assert!(!ipv4_ok("999.0.0.1"));
    }

    #[test]
    fn phone_requires_plausible_digit_count() {
        assert!(phone_ok("+81-90-1234-5678"));
        assert!(!phone_ok("12345"));
    }
}
