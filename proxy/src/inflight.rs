//! in-flight 呼び出しの保持。
//!
//! `/next` でイベントを観測してから `/response`・`/error` で確定するまでの間、
//! イベントをメモリに置く。SHUTDOWN（timeout）時には残っているものを
//! 「応答が返らなかった失敗」としてフラッシュする。

use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use time::OffsetDateTime;

/// イベント本体の保持形式。Runtime API のボディは JSON とは限らず、
/// UTF-8 ですらない場合があるため 3 系統を区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventEncoding {
    /// 通常の JSON イベント。
    Json,
    /// 非 JSON だが UTF-8 文字列として保持できる生テキスト。
    /// レコードへ `eventIsRawText` として写す。
    RawText,
    /// UTF-8 ですらないバイナリ。base64 文字列として保持し、
    /// レコードへ `eventIsBase64` として写す（lossy 変換を避ける）。
    Base64,
}

#[derive(Debug)]
pub struct Invocation {
    pub request_id: String,
    /// イベント本文。encoding が RawText/Base64 のときは文字列 Value。
    pub event: Value,
    pub encoding: EventEncoding,
    pub invoked_at: OffsetDateTime,
}

/// 記録済み requestId の保持上限。長寿命環境で失敗が積み上がっても
/// 無制限に増えないよう FIFO で間引く。dedupe の効く実用上の窓
/// （同一 requestId の再試行間隔）より十分に大きい。
const RECORDED_CAP: usize = 4096;

#[derive(Debug, Default)]
struct State {
    map: HashMap<String, Invocation>,
    /// 記録済み requestId。同じ呼び出しの失敗レコードは 1 件 —
    /// /error のリトライや SHUTDOWN drain との競合で同一 S3 キーへ
    /// event 欠落・errorContext 欠落の記録が上書きするのを防ぐ。
    recorded: HashSet<String>,
    /// recorded の FIFO 順。cap 超過時に古い方から捨てる。
    recorded_order: VecDeque<String>,
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

    /// イベントの取り出しと記録権の確保を 1 回のロックで行う。
    /// `remove` と `claim_record` を別呼び出しにすると、forward 中に
    /// 到着した 2 回目の /error が event 欠落のまま記録権を取り、
    /// イベント保持側の記録を捨てさせる競合がある。
    /// 戻り値は (取り出した呼び出し, 記録権を得たか)。
    pub fn remove_and_claim(&self, request_id: &str) -> (Option<Invocation>, bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let inv = st.map.remove(request_id);
        let claimed = st.recorded.insert(request_id.to_string());
        if claimed {
            st.recorded_order.push_back(request_id.to_string());
            while st.recorded_order.len() > RECORDED_CAP {
                if let Some(old) = st.recorded_order.pop_front() {
                    st.recorded.remove(&old);
                }
            }
        }
        (inv, claimed)
    }

    /// この requestId の失敗記録をまだ出していなければ記録権を得る。
    /// 2 度目以降の呼び出しは false（呼び出し側は保存をスキップする）。
    /// map から外れた呼び出しでも有効 — init_error や /error リトライの
    /// 重複防止に使う。
    pub fn claim_record(&self, request_id: &str) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.recorded.insert(request_id.to_string()) {
            st.recorded_order.push_back(request_id.to_string());
            while st.recorded_order.len() > RECORDED_CAP {
                if let Some(old) = st.recorded_order.pop_front() {
                    st.recorded.remove(&old);
                }
            }
            true
        } else {
            false
        }
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
            encoding: EventEncoding::Json,
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
    fn remove_and_claim_is_atomic_per_request() {
        let f = InFlight::new();
        f.insert(inv("r1"));
        let (i, claimed) = f.remove_and_claim("r1");
        assert!(i.is_some() && claimed);
        // 2 回目はイベントも記録権も取れない
        let (i2, claimed2) = f.remove_and_claim("r1");
        assert!(i2.is_none() && !claimed2);
    }

    #[test]
    fn recorded_cap_evicts_oldest() {
        let f = InFlight::new();
        for i in 0..RECORDED_CAP {
            assert!(f.claim_record(&format!("r{i}")));
        }
        assert!(!f.claim_record("r0"));
        // cap を超えると最古の r0 が追い出され、再度 claim できる
        assert!(f.claim_record("overflow"));
        assert!(f.claim_record("r0"));
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
