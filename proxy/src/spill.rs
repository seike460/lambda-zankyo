//! /tmp 退避（spill）の管理。
//! S3 へ届かない間にレコードを失わないよう、record JSON をローカルへ
//! 書き、起動時に再送する。/tmp を使い尽くさないよう、書き込み時と
//! 回収時の両方でファイル数の上限を適用する。

use crate::record::s3_key_from_parts;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

/// `.part` 残滓を掃除してよい経過時間。書き込み途中のファイルを
/// 消さないための猶予（クラッシュ後の孤児だけを拾う）。
const PART_ORPHAN_GRACE: Duration = Duration::from_secs(60);

/// このツールが管理する spill ファイル名の接頭辞。
/// ZANKYO_SPILL_DIR を共有ディレクトリ（/tmp 直下等）に向けた設定でも、
/// 他ツール・他ユーザのファイルを回収・削除対象にしないための印。
const MANAGED_PREFIX: &str = "zankyo-";

/// 退避ファイルを書き、上限を超えたら古いものから捨てる。
/// `.part` へ書いてから rename する: 定期回収が書き込み途中の
/// 半端な JSON を読んで「復旧不能」として消す競合を防ぐ。
pub fn write(dir: &str, max_files: usize, request_id: &str, body: &[u8]) {
    let dir_path = Path::new(dir);
    let path = dir_path.join(filename(request_id));
    let tmp = dir_path.join(format!(".{}.part", filename(request_id)));
    let result = std::fs::create_dir_all(dir_path)
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, &path));
    match result {
        Ok(()) => {
            info!(request_id, path = %path.display(), "record spilled to /tmp");
            enforce_cap(dir_path, max_files);
        }
        Err(e) => warn!(request_id, error = %e, "failed to spill record"),
    }
}

/// 回収待ちの spill を (path, body, s3_key) で列挙する。
/// 壊れた JSON や必須フィールド欠落は永遠に復旧できないため、
/// ここで削除する（残すと rerun のたびに積み上がる stale 残滓になる）。
/// `max_age` を超えたファイルも再送せず破棄する（古い記録の遅れ再送は
/// 保存先の時系列を混乱させるだけで復旧価値が薄い）。
pub fn pending(dir: &Path, max_age: Duration) -> Vec<(PathBuf, Vec<u8>, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // ディレクトリ自体が無い = 退避なし
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        // 自ツールの接頭辞を持たないファイルは一切触らない
        // （共有ディレクトリ指定時の他者ファイルを消さない）。
        if ext == Some("part") {
            if is_managed_part(&path) {
                sweep_orphan_part(&path);
            }
            continue;
        }
        if ext != Some("json") || !is_managed(&path) {
            continue;
        }
        if is_expired(&path, max_age) {
            if std::fs::remove_file(&path).is_ok() {
                warn!(path = %path.display(), "dropping expired spilled record");
            }
            continue;
        }
        let Ok(body) = std::fs::read(&path) else {
            continue;
        };
        match key_for_spilled(&body) {
            Some(key) => out.push((path, body, key)),
            None => {
                if std::fs::remove_file(&path).is_ok() {
                    warn!(path = %path.display(), "dropping unrecoverable spilled file");
                }
            }
        }
    }
    out
}

/// ファイルの経過時間が `max_age` を超えているか。
/// mtime が取れない/未来の場合は期限切れとみなさない（消しすぎない）。
fn is_expired(path: &Path, max_age: Duration) -> bool {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > max_age)
}

/// クラッシュで残った `.part` を消す。猶予時間内のものは
/// 書き込み途中かもしれないので触らない。
fn sweep_orphan_part(path: &Path) {
    if is_expired(path, PART_ORPHAN_GRACE) && std::fs::remove_file(path).is_ok() {
        warn!(path = %path.display(), "dropping orphaned partial spill");
    }
}

/// ファイル名が自ツールの管理対象（spill JSON）か。
fn is_managed(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(MANAGED_PREFIX))
}

/// `.part` 側の管理対象判定。書き込み中名は `.zankyo-*.json.part`。
fn is_managed_part(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(&format!(".{MANAGED_PREFIX}")))
}

/// spill dir の JSON ファイル数を `cap` 以下に抑える。
/// S3 が届かない状態が続いても /tmp を使い尽くさないよう、
/// 更新時刻の古いものから捨てる（新しい記録ほど復旧価値が高い前提）。
/// 接頭辞を持たないファイルは数えも消しもしない。
pub fn enforce_cap(dir: &Path, cap: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json") && is_managed(p))
        .map(|p| {
            let mtime = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            (mtime, p)
        })
        .collect();
    if files.len() <= cap {
        return;
    }
    files.sort_by_key(|(mtime, _)| *mtime);
    for (_, path) in files.iter().take(files.len() - cap) {
        if std::fs::remove_file(path).is_ok() {
            warn!(path = %path.display(), "spill cap reached; dropping oldest record");
        }
    }
}

/// requestId は外部入力（Runtime API ヘッダ）由来なので、ファイル名に
/// 使える文字だけへ正規化する。`/` や `..` を含む値で spill_dir の
/// 外へ書き出さないための防御。
pub fn filename(request_id: &str) -> String {
    let clean: String = request_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.');
    let name = if clean.is_empty() { "record" } else { clean };
    format!("{MANAGED_PREFIX}{name}.json")
}

/// spill した record JSON から S3 キーを再構成する。
/// invokedAt は `yyyy-mm-ddTHH:MM:SSZ` 固定長なので日付部分だけ切り出す。
/// レイアウト本体は record::s3_key_from_parts と共有する。
fn key_for_spilled(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let function = v.get("functionName")?.as_str()?;
    let request_id = v.get("requestId")?.as_str()?;
    let invoked_at = v.get("invokedAt")?.as_str()?;
    let (y, m, d) = (
        invoked_at.get(0..4)?.parse::<i32>().ok()?,
        invoked_at.get(5..7)?.parse::<u8>().ok()?,
        invoked_at.get(8..10)?.parse::<u8>().ok()?,
    );
    Some(s3_key_from_parts(function, y, m, d, request_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_spilled_rebuilds_layout() {
        let body = br#"{"functionName":"fn","requestId":"r1","invokedAt":"2026-09-22T01:02:03Z"}"#;
        assert_eq!(
            key_for_spilled(body),
            Some("zankyo/fn/2026/09/22/r1.json".to_string())
        );
    }

    #[test]
    fn key_for_spilled_rejects_malformed() {
        assert_eq!(key_for_spilled(b"not json"), None);
        assert_eq!(key_for_spilled(br#"{"functionName":"fn"}"#), None);
    }

    #[test]
    fn spill_filename_strips_path_separators() {
        assert_eq!(filename("req-123"), "zankyo-req-123.json");
        // `.` `/` `\` は全て `_` へ潰れるので traversal できない
        assert_eq!(filename("../../etc/passwd"), "zankyo-______etc_passwd.json");
        assert_eq!(filename("a/b\\c"), "zankyo-a_b_c.json");
        assert_eq!(filename(""), "zankyo-record.json");
        assert_eq!(filename("../.."), "zankyo-_____.json");
    }

    #[test]
    fn spill_cap_drops_oldest_files() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("zankyo-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 古→新の順に 3 ファイル、mtime を明示して順序を確定させる
        for i in 0..3u64 {
            let p = dir.join(format!("zankyo-f{i}.json"));
            std::fs::write(&p, b"{}").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(i + 1))
                .unwrap();
        }
        enforce_cap(&dir, 2);
        // 最古の f0 だけが消え、新しい 2 つが残る
        assert!(!dir.join("zankyo-f0.json").exists());
        assert!(dir.join("zankyo-f1.json").exists());
        assert!(dir.join("zankyo-f2.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_leaves_no_part_files() {
        let dir = std::env::temp_dir().join(format!("zankyo-atomic-{}", std::process::id()));
        write(dir.to_str().unwrap(), 8, "req-9", b"{}");
        // rename 済みなら .json だけが残り .part は残らない
        assert!(dir.join("zankyo-req-9.json").exists());
        let parts: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("part"))
            .collect();
        assert!(parts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_sweeps_old_part_orphans() {
        let dir = std::env::temp_dir().join(format!("zankyo-part-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // クラッシュ残滓の .part を古い mtime で置く
        let orphan = dir.join(".zankyo-dead.json.part");
        std::fs::write(&orphan, b"{\"partial\":").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let items = pending(&dir, Duration::from_secs(3600));
        assert!(items.is_empty());
        assert!(!orphan.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_drops_expired_records() {
        let dir = std::env::temp_dir().join(format!("zankyo-exp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("zankyo-old.json");
        std::fs::write(
            &old,
            br#"{"functionName":"fn","requestId":"r1","invokedAt":"2026-09-22T01:02:03Z"}"#,
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        // max_age 1 時間では期限切れ → 再送対象にならず消える
        let items = pending(&dir, Duration::from_secs(3600));
        assert!(items.is_empty());
        assert!(!old.exists());
        // max_age が十分大きければ再送対象になる
        std::fs::write(
            &old,
            br#"{"functionName":"fn","requestId":"r1","invokedAt":"2026-09-22T01:02:03Z"}"#,
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let items = pending(&dir, Duration::from_secs(u64::MAX / 2));
        assert_eq!(items.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_drops_unrecoverable_files() {
        let dir = std::env::temp_dir().join(format!("zankyo-pend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 回収できるもの・壊れた JSON・フィールド欠落の 3 種を置く
        let good = dir.join("zankyo-good.json");
        std::fs::write(
            &good,
            br#"{"functionName":"fn","requestId":"r1","invokedAt":"2026-09-22T01:02:03Z"}"#,
        )
        .unwrap();
        let broken = dir.join("zankyo-broken.json");
        std::fs::write(&broken, b"not json").unwrap();
        let missing = dir.join("zankyo-missing.json");
        std::fs::write(&missing, br#"{"functionName":"fn"}"#).unwrap();

        let items = pending(&dir, Duration::from_secs(3600));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].2, "zankyo/fn/2026/09/22/r1.json");
        // 復旧不能なものはその場で消え、次回以降残滓として残らない
        assert!(!broken.exists());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_files_are_never_touched() {
        // ZANKYO_SPILL_DIR を共有ディレクトリに向けた設定でも、
        // 接頭辞を持たない他者のファイルは回収・掃除・cap の対象外。
        let dir = std::env::temp_dir().join(format!("zankyo-foreign-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let foreign_json = dir.join("other-tool.json");
        std::fs::write(&foreign_json, b"not json").unwrap();
        let foreign_part = dir.join(".other.json.part");
        std::fs::write(&foreign_part, b"{\"partial\":").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&foreign_part)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();

        let items = pending(&dir, Duration::from_secs(3600));
        assert!(items.is_empty());
        enforce_cap(&dir, 0);
        assert!(foreign_json.exists());
        assert!(foreign_part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
