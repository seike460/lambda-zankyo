//! 文書とコードの突き合わせ。
//! 設定で変えない固定の上限・タイムアウト・間隔（`proxy/src` の数値や時間の定数）と、
//! ARCHITECTURE.md の「固定の安全上限」の表が一致することを確かめる。
//! 設定の既定値（`DEFAULT_*`）は README の設定表が扱うので、ここでは見ない。

use std::collections::BTreeSet;
use std::path::Path;

const SECTION: &str = "## 固定の安全上限";

/// ソースから、数値か時間を型に持つ定数の名前を集める。テストのモジュールは見ない。
fn fixed_consts(src: &str) -> BTreeSet<String> {
    let body = src.split("#[cfg(test)]").next().unwrap_or(src);
    body.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = line
                .strip_prefix("pub(crate) ")
                .or_else(|| line.strip_prefix("pub "))
                .unwrap_or(line);
            let (name, rest) = line.strip_prefix("const ")?.split_once(':')?;
            let ty = rest.split('=').next()?.trim();
            let numeric = matches!(ty, "u32" | "u64" | "usize" | "i64") || ty.ends_with("Duration");
            (numeric && !name.starts_with("DEFAULT_")).then(|| name.trim().to_string())
        })
        .collect()
}

/// 表の 1 列目（`| \`NAME\` |`）に並ぶ定数の名前を集める。
fn listed_consts(doc: &str) -> BTreeSet<String> {
    let start = doc
        .find(SECTION)
        .expect("ARCHITECTURE.md に固定の安全上限の節が要る");
    let section = &doc[start + SECTION.len()..];
    let section = &section[..section.find("\n## ").unwrap_or(section.len())];
    section
        .lines()
        .filter_map(|line| {
            let name = line.strip_prefix("| `")?.split('`').next()?;
            Some(name.to_string())
        })
        .collect()
}

#[test]
fn fixed_caps_match_architecture_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut in_code = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            in_code.extend(fixed_consts(&std::fs::read_to_string(&path).unwrap()));
        }
    }
    let doc = std::fs::read_to_string(root.join("../ARCHITECTURE.md")).unwrap();
    let in_doc = listed_consts(&doc);
    // 走査が空振りしていないこと（既知の定数を拾えている）
    assert!(in_code.contains("EXT_BODY_TIMEOUT"), "{in_code:?}");
    assert_eq!(
        in_code, in_doc,
        "proxy/src の固定値と ARCHITECTURE.md の表が食い違う"
    );
}
