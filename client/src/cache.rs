use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct CacheEntry<V> {
    value: V,
    inserted_at: Instant,
    last_used: u64,
}

#[derive(Debug)]
pub struct TtlLruCache<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
    max_entries: usize,
    ttl: Duration,
    access_counter: u64,
}

impl<K, V> TtlLruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        TtlLruCache {
            entries: HashMap::new(),
            max_entries,
            ttl,
            access_counter: 0,
        }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        if self
            .entries
            .get(key)
            .is_some_and(|entry| entry.inserted_at.elapsed() > self.ttl)
        {
            self.entries.remove(key);
            return None;
        }

        self.access_counter += 1;
        self.entries.get_mut(key).map(|entry| {
            entry.last_used = self.access_counter;
            entry.value.clone()
        })
    }

    pub fn insert(&mut self, key: K, value: V) {
        if self.max_entries == 0 {
            return;
        }

        self.access_counter += 1;
        self.entries.insert(
            key,
            CacheEntry {
                value,
                inserted_at: Instant::now(),
                last_used: self.access_counter,
            },
        );
        self.evict_expired();
        self.evict_lru();
    }

    pub fn remove(&mut self, key: &K) {
        self.entries.remove(key);
    }

    pub fn remove_matching<F>(&mut self, predicate: F)
    where
        F: Fn(&K) -> bool,
    {
        self.entries.retain(|key, _| !predicate(key));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn evict_expired(&mut self) {
        let ttl = self.ttl;
        self.entries
            .retain(|_, entry| entry.inserted_at.elapsed() <= ttl);
    }

    fn evict_lru(&mut self) {
        while self.entries.len() > self.max_entries {
            let key_to_remove = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());

            if let Some(key) = key_to_remove {
                self.entries.remove(&key);
            } else {
                break;
            }
        }
    }
}
