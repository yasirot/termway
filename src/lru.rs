//! LRU map with pinning and cost accounting.
//!
//! Pinned entries (tiles currently on screen) are never evicted; recency of an
//! unpinned entry is the time it was last used or unpinned.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

#[derive(Debug, Clone, Copy)]
struct Entry {
    value: u32,
    cost: u64,
    pins: u32,
    tick: u64,
}

#[derive(Debug)]
pub struct Lru<K: Hash + Eq + Copy> {
    map: HashMap<K, Entry>,
    /// Unpinned entries ordered by recency.
    order: BTreeMap<u64, K>,
    tick: u64,
    total_cost: u64,
}

impl<K: Hash + Eq + Copy> Default for Lru<K> {
    fn default() -> Self {
        Self { map: HashMap::new(), order: BTreeMap::new(), tick: 0, total_cost: 0 }
    }
}

impl<K: Hash + Eq + Copy> Lru<K> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn total_cost(&self) -> u64 {
        self.total_cost
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Looks up a key and marks it as recently used.
    pub fn get(&mut self, key: &K) -> Option<u32> {
        let tick = self.next_tick();
        let e = self.map.get_mut(key)?;
        if e.pins == 0 {
            self.order.remove(&e.tick);
            self.order.insert(tick, *key);
        }
        e.tick = tick;
        Some(e.value)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Inserts an unpinned entry. Panics if the key is already present, since
    /// that would leak the old terminal image.
    pub fn insert(&mut self, key: K, value: u32, cost: u64) {
        let tick = self.next_tick();
        let prev = self.map.insert(key, Entry { value, cost, pins: 0, tick });
        assert!(prev.is_none(), "duplicate LRU key");
        self.order.insert(tick, key);
        self.total_cost += cost;
    }

    pub fn pin(&mut self, key: &K) {
        let e = self.map.get_mut(key).expect("pin of unknown key");
        if e.pins == 0 {
            self.order.remove(&e.tick);
        }
        e.pins += 1;
    }

    pub fn unpin(&mut self, key: &K) {
        self.tick += 1;
        let tick = self.tick;
        let e = self.map.get_mut(key).expect("unpin of unknown key");
        assert!(e.pins > 0, "unpin of unpinned key");
        e.pins -= 1;
        if e.pins == 0 {
            e.tick = tick;
            self.order.insert(tick, *key);
        }
    }

    pub fn pins(&self, key: &K) -> u32 {
        self.map.get(key).map_or(0, |e| e.pins)
    }

    /// Evicts least recently used unpinned entries until the total cost is within
    /// `limit`, returning the evicted (key, value) pairs oldest first.
    pub fn evict_to(&mut self, limit: u64) -> Vec<(K, u32)> {
        let mut evicted = Vec::new();
        while self.total_cost > limit {
            let Some((_, key)) = self.order.pop_first() else { break };
            let e = self.map.remove(&key).expect("order and map out of sync");
            self.total_cost -= e.cost;
            evicted.push((key, e.value));
        }
        evicted
    }

    /// Removes everything, returning all values.
    pub fn drain(&mut self) -> Vec<u32> {
        self.order.clear();
        self.total_cost = 0;
        self.map.drain().map(|(_, e)| e.value).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_first() {
        let mut l = Lru::new();
        l.insert(1u64, 10, 1);
        l.insert(2, 20, 1);
        l.insert(3, 30, 1);
        assert_eq!(l.get(&1), Some(10));
        assert_eq!(l.evict_to(1), vec![(2, 20), (3, 30)]);
        assert_eq!(l.len(), 1);
        assert_eq!(l.total_cost(), 1);
        assert_eq!(l.get(&2), None);
    }

    #[test]
    fn pinned_entries_survive_eviction() {
        let mut l = Lru::new();
        l.insert(1u64, 10, 5);
        l.insert(2, 20, 5);
        l.pin(&1);
        l.pin(&1);
        assert_eq!(l.evict_to(0), vec![(2, 20)]);
        assert_eq!(l.total_cost(), 5, "limit cannot be met while pinned");
        l.unpin(&1);
        assert!(l.evict_to(0).is_empty(), "still pinned once");
        l.unpin(&1);
        assert_eq!(l.evict_to(0), vec![(1, 10)]);
        assert!(l.is_empty());
    }

    #[test]
    fn unpin_refreshes_recency() {
        let mut l = Lru::new();
        l.insert(1u64, 10, 1);
        l.insert(2, 20, 1);
        l.pin(&1);
        l.unpin(&1);
        assert_eq!(l.evict_to(1), vec![(2, 20)]);
    }

    #[test]
    fn get_on_pinned_keeps_it_out_of_order() {
        let mut l = Lru::new();
        l.insert(1u64, 10, 1);
        l.pin(&1);
        assert_eq!(l.get(&1), Some(10));
        assert!(l.evict_to(0).is_empty());
        assert_eq!(l.pins(&1), 1);
    }

    #[test]
    fn drain_returns_everything() {
        let mut l = Lru::new();
        l.insert(1u64, 10, 3);
        l.insert(2, 20, 4);
        l.pin(&2);
        let mut v = l.drain();
        v.sort();
        assert_eq!(v, vec![10, 20]);
        assert_eq!((l.len(), l.total_cost()), (0, 0));
        assert!(l.evict_to(0).is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate LRU key")]
    fn duplicate_insert_panics() {
        let mut l = Lru::new();
        l.insert(1u64, 1, 1);
        l.insert(1, 2, 1);
    }
}
