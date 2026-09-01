//! Bounded in-memory caches (ADR-0005).
//!
//! The control plane stores resolved effective configuration so the proxy path
//! avoids re-merging the tenant chain on every request. Entries are keyed by
//! the alias and the owning tenant chain; capacity is bounded so a busy
//! multi-tenant deployment cannot grow the table without limit. Upstream
//! *responses* are never cached (DESIGN §4.1).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// A bounded, TTL-aware map with insertion-order eviction.
#[derive(Debug)]
pub struct BoundedCache<K, V> {
    capacity: usize,
    ttl: Option<Duration>,
    entries: Mutex<HashMap<K, Arc<Cached<V>>>>,
    order: Mutex<Vec<K>>,
}

#[derive(Debug)]
struct Cached<V> {
    value: V,
    stored_at: Instant,
}

impl<K, V> BoundedCache<K, V>
where
    K: Eq + Hash + PartialEq + Clone + Ord,
    V: Clone,
{
    /// Creates a cache holding at most `capacity` entries, each expiring after
    /// `ttl` when given.
    #[must_use]
    pub fn new(capacity: usize, ttl: Option<Duration>) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    /// Stores a value, evicting the oldest entry when over capacity.
    pub fn insert(&self, key: K, value: V) {
        let mut entries = self.entries.lock();
        entries.insert(
            key.clone(),
            Arc::new(Cached {
                value,
                stored_at: Instant::now(),
            }),
        );
        let mut order = self.order.lock();
        order.retain(|existing| existing != &key);
        order.push(key);
        while entries.len() > self.capacity {
            let Some(oldest) = order.first().cloned() else {
                break;
            };
            entries.remove(&oldest);
            order.remove(0);
        }
    }

    /// Fetches a value, honouring the TTL.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<V> {
        let entries = self.entries.lock();
        let cached = entries.get(key)?;
        if self.ttl.is_some_and(|ttl| cached.stored_at.elapsed() > ttl) {
            return None;
        }
        Some(cached.value.clone())
    }

    /// Drops one entry.
    pub fn invalidate(&self, key: &K) {
        self.entries.lock().remove(key);
        self.order.lock().retain(|existing| existing != key);
    }

    /// Drops every entry (control-plane writes invalidate the table).
    pub fn clear(&self) {
        self.entries.lock().clear();
        self.order.lock().clear();
    }

    /// Number of live entries (test and metrics helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// `true` when the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared cache handle.
pub type SharedConfigCache<V> = Arc<BoundedCache<String, V>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> BoundedCache<String, u32> {
        BoundedCache::new(2, None)
    }

    #[test]
    fn evicts_the_oldest_entry_beyond_capacity() {
        let cache = cache();
        cache.insert("a".to_owned(), 1);
        cache.insert("b".to_owned(), 2);
        cache.insert("c".to_owned(), 3);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"a".to_owned()), None);
        assert_eq!(cache.get(&"b".to_owned()), Some(2));
        assert_eq!(cache.get(&"c".to_owned()), Some(3));
    }

    #[test]
    fn reinserting_refreshes_the_order() {
        let cache = cache();
        cache.insert("a".to_owned(), 1);
        cache.insert("b".to_owned(), 2);
        cache.insert("a".to_owned(), 10);
        cache.insert("c".to_owned(), 3);
        assert_eq!(cache.get(&"a".to_owned()), Some(10));
        assert_eq!(cache.get(&"b".to_owned()), None);
    }

    #[test]
    fn invalidation_removes_one_entry() {
        let cache = cache();
        cache.insert("a".to_owned(), 1);
        cache.invalidate(&"a".to_owned());
        assert_eq!(cache.get(&"a".to_owned()), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn expired_entries_are_not_served() {
        let cache: BoundedCache<String, u32> = BoundedCache::new(2, Some(Duration::from_millis(1)));
        cache.insert("a".to_owned(), 1);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get(&"a".to_owned()), None);
    }
}
