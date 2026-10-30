//! The AI worker's answer cache (SPEC 16.5): an in-memory LRU keyed by `(account, message id)`,
//! so reopening a message costs nothing. Never written to disk.

use std::collections::{HashMap, VecDeque};

use super::LabelAnswers;
use super::jev::Verdict;
use crate::model::AccountId;

/// How many messages' answers are kept.
pub const CACHE_SIZE: usize = 1000;

pub type CacheKey = (AccountId, String);

/// What is known about one message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CacheEntry {
    pub verdict: Option<Verdict>,
    /// The label fingerprint (`None` for a partial answer) and the probabilities (M17).
    pub labels: Option<LabelAnswers>,
}

pub struct DecisionCache {
    cap: usize,
    map: HashMap<CacheKey, CacheEntry>,
    /// Least recently used first.
    order: VecDeque<CacheKey>,
}

impl DecisionCache {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn touch(&mut self, key: &CacheKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
    }

    /// The entry, marked as recently used.
    pub fn get(&mut self, key: &CacheKey) -> Option<CacheEntry> {
        let entry = self.map.get(key).cloned()?;
        self.touch(key);
        Some(entry)
    }

    /// Whether the entry answers what a check asks: the verdict when `verdict`, and labels
    /// under fingerprint `labels` when that is `Some`. A changed fingerprint re-asks labels
    /// only.
    pub fn covers(&self, key: &CacheKey, verdict: bool, labels: Option<u64>) -> bool {
        let Some(e) = self.map.get(key) else {
            return false;
        };
        let verdict_ok = !verdict || e.verdict.is_some();
        let labels_ok = match labels {
            None => true,
            Some(fp) => matches!(&e.labels, Some((Some(have), _)) if *have == fp),
        };
        verdict_ok && labels_ok
    }

    /// Stores an answer, merging with what is there: a labels-only answer keeps the verdict.
    pub fn insert(&mut self, key: CacheKey, entry: CacheEntry) {
        let merged = match self.map.remove(&key) {
            Some(old) => CacheEntry {
                verdict: entry.verdict.or(old.verdict),
                labels: entry.labels.or(old.labels),
            },
            None => entry,
        };
        self.map.insert(key.clone(), merged);
        self.touch(&key);
        while self.map.len() > self.cap {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&oldest);
        }
    }

    pub fn forget_account(&mut self, account: &str) {
        self.map.retain(|(a, _), _| a != account);
        self.order.retain(|(a, _)| a != account);
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::jev::{THRESHOLDS, verdict_from};

    fn key(a: &str, m: &str) -> CacheKey {
        (a.to_string(), m.to_string())
    }

    fn verdict() -> CacheEntry {
        CacheEntry {
            verdict: Some(verdict_from(0.9, 0.0, 0.0, &THRESHOLDS)),
            labels: None,
        }
    }

    #[test]
    fn least_recently_used_goes_first() {
        let mut c = DecisionCache::new(2);
        c.insert(key("a", "1"), verdict());
        c.insert(key("a", "2"), verdict());
        assert!(c.get(&key("a", "1")).is_some());
        c.insert(key("a", "3"), verdict());
        assert_eq!(c.len(), 2);
        assert!(c.get(&key("a", "2")).is_none());
        assert!(c.get(&key("a", "1")).is_some());
    }

    #[test]
    fn forget_account_drops_only_that_account() {
        let mut c = DecisionCache::new(10);
        c.insert(key("a", "1"), verdict());
        c.insert(key("b", "1"), verdict());
        c.forget_account("a");
        assert!(!c.covers(&key("a", "1"), true, None));
        assert!(c.covers(&key("b", "1"), true, None));
        c.clear();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn a_fingerprint_change_reasks_labels_only() {
        let mut c = DecisionCache::new(10);
        let k = key("a", "1");
        c.insert(
            k.clone(),
            CacheEntry {
                verdict: verdict().verdict,
                labels: Some((Some(7), vec![("Label_1".into(), 0.9)])),
            },
        );
        assert!(c.covers(&k, true, Some(7)));
        assert!(!c.covers(&k, true, Some(8)));
        assert!(c.covers(&k, true, None), "the verdict is still cached");
        // A labels-only answer keeps the stored verdict.
        c.insert(
            k.clone(),
            CacheEntry {
                verdict: None,
                labels: Some((Some(8), vec![])),
            },
        );
        assert!(c.covers(&k, true, Some(8)));
        // A partial answer never covers.
        c.insert(
            k.clone(),
            CacheEntry {
                verdict: None,
                labels: Some((None, vec![])),
            },
        );
        assert!(!c.covers(&k, false, Some(8)));
    }
}
