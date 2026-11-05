//! Byte-bounded LRU. Eviction scans for the oldest stamp, which is fine for
//! the few hundred events a track keeps hot and only runs when full.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

pub(crate) struct Lru<K, V> {
    map: HashMap<K, (Arc<V>, u64, usize)>,
    clock: u64,
    bytes: usize,
    cap: usize,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
            bytes: 0,
            cap: cap_bytes,
        }
    }

    pub fn get(&mut self, k: &K) -> Option<Arc<V>> {
        self.clock += 1;
        let clock = self.clock;
        let e = self.map.get_mut(k)?;
        e.1 = clock;
        Some(Arc::clone(&e.0))
    }

    /// Inserts, then evicts oldest entries until within the cap. The entry
    /// just inserted is never evicted, so one oversized value still works.
    pub fn insert(&mut self, k: K, v: Arc<V>, bytes: usize) {
        self.clock += 1;
        if let Some(old) = self.map.insert(k.clone(), (v, self.clock, bytes)) {
            self.bytes -= old.2;
        }
        self.bytes += bytes;
        while self.bytes > self.cap && self.map.len() > 1 {
            let victim = self
                .map
                .iter()
                .filter(|(key, _)| **key != k)
                .min_by_key(|(_, e)| e.1)
                .map(|(key, _)| key.clone());
            match victim.and_then(|key| self.map.remove(&key)) {
                Some(old) => self.bytes -= old.2,
                None => break,
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_and_stays_bounded() {
        let mut c: Lru<u32, u8> = Lru::new(100);
        for i in 0..10 {
            c.insert(i, Arc::new(0), 30);
            assert!(c.bytes() <= 100);
        }
        assert_eq!(c.len(), 3);
        assert!(c.get(&9).is_some());
        assert!(c.get(&0).is_none());
    }

    #[test]
    fn get_refreshes_recency() {
        let mut c: Lru<u32, u8> = Lru::new(60);
        c.insert(1, Arc::new(0), 30);
        c.insert(2, Arc::new(0), 30);
        assert!(c.get(&1).is_some());
        c.insert(3, Arc::new(0), 30);
        assert!(c.get(&1).is_some());
        assert!(c.get(&2).is_none());
    }

    #[test]
    fn oversized_entry_is_kept_alone() {
        let mut c: Lru<u32, u8> = Lru::new(10);
        c.insert(1, Arc::new(0), 5);
        c.insert(2, Arc::new(0), 50);
        assert_eq!(c.len(), 1);
        assert!(c.get(&2).is_some());
    }
}
