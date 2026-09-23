//! in-flight 呼び出しの保持。
//!
//! `/next` でイベントを観測してから `/response`・`/error` で確定するまでの間、
//! イベントをメモリに置く。SHUTDOWN（timeout）時には残っているものを
//! 「応答が返らなかった失敗」としてフラッシュする。

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use time::OffsetDateTime;

#[derive(Debug)]
pub struct Invocation {
    pub request_id: String,
    /// イベント本文。JSON として解釈できない場合は生文字列を保持する。
    pub event: Value,
    /// `event` が生テキスト（非 JSON ボディ）由来か。
    /// レコードへ `eventIsRawText` として写し、CLI が再送時に
    /// JSON 再エンコードせず原文を送れるようにする。
    pub event_is_raw: bool,
    pub invoked_at: OffsetDateTime,
}

#[derive(Debug, Default)]
struct State {
    map: HashMap<String, Invocation>,
    /// 記録済み requestId。同じ呼び出しの失敗レコードは 1 件 —
    /// /error のリトライや SHUTDOWN drain との競合で同一 S3 キーへ
    /// event 欠落・errorContext 欠落の記録が上書きするのを防ぐ。
    recorded: HashSet<String>,
}

#[derive(Debug, Default)]
pub struct InFlight {
    state: Mutex<State>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, inv: Invocation) {
        let id = inv.request_id.clone();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .insert(id, inv);
    }

    pub fn remove(&self, request_id: &str) -> Option<Invocation> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .remove(request_id)
    }

    /// この requestId の失敗記録をまだ出していなければ記録権を得る。
    /// 2 度目以降の呼び出しは false（呼び出し側は保存をスキップする）。
    /// map から外れた呼び出しでも有効 — /error リトライの重複防止に使う。
    pub fn claim_record(&self, request_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recorded
            .insert(request_id.to_string())
    }

    /// SHUTDOWN 時に残っている全呼び出しを取り出す。
    pub fn drain(&self) -> Vec<Invocation> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.map.drain().map(|(_, v)| v).collect()
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(id: &str) -> Invocation {
        Invocation {
            request_id: id.to_string(),
            event: serde_json::json!({"k": 1}),
            event_is_raw: false,
            invoked_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn insert_remove_roundtrip() {
        let f = InFlight::new();
        f.insert(inv("r1"));
        f.insert(inv("r2"));
        assert_eq!(f.len(), 2);
        assert!(f.remove("r1").is_some());
        assert!(f.remove("r1").is_none());
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn claim_record_is_once_per_request_id() {
        let f = InFlight::new();
        assert!(f.claim_record("r1"));
        assert!(!f.claim_record("r1"));
        assert!(f.claim_record("r2"));
    }

    #[test]
    fn drain_empties_map() {
        let f = InFlight::new();
        f.insert(inv("r1"));
        f.insert(inv("r2"));
        let drained = f.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(f.len(), 0);
    }
}
