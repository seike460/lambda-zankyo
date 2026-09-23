//! in-flight 呼び出しの保持。
//!
//! `/next` でイベントを観測してから `/response`・`/error` で確定するまでの間、
//! イベントをメモリに置く。SHUTDOWN（timeout）時には残っているものを
//! 「応答が返らなかった失敗」としてフラッシュする。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use time::OffsetDateTime;

#[derive(Debug)]
pub struct Invocation {
    pub request_id: String,
    /// イベント本文。JSON として解釈できない場合は生文字列を保持する。
    pub event: Value,
    pub invoked_at: OffsetDateTime,
}

#[derive(Debug, Default)]
pub struct InFlight {
    map: Mutex<HashMap<String, Invocation>>,
}

impl InFlight {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, inv: Invocation) {
        let id = inv.request_id.clone();
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, inv);
    }

    pub fn remove(&self, request_id: &str) -> Option<Invocation> {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_id)
    }

    /// SHUTDOWN 時に残っている全呼び出しを取り出す。
    pub fn drain(&self) -> Vec<Invocation> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.drain().map(|(_, v)| v).collect()
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
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
    fn drain_empties_map() {
        let f = InFlight::new();
        f.insert(inv("r1"));
        f.insert(inv("r2"));
        let drained = f.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(f.len(), 0);
    }
}
