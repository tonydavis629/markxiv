use lru::LruCache;
use std::num::NonZeroUsize;

// A thin wrapper around LruCache for markdown per arXiv id
pub struct MkCache(LruCache<String, String>);

impl MkCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap();
        Self(LruCache::new(cap))
    }

    pub fn get(&mut self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }

    pub fn put(&mut self, key: String, value: String) {
        self.0.put(key, value);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_basic() {
        let mut c = MkCache::new(2);
        c.put("a".into(), "1".into());
        c.put("b".into(), "2".into());
        assert_eq!(c.get("a").as_deref(), Some("1"));
        c.put("c".into(), "3".into()); // evicts least-recently used (b) after accessing a
        assert!(c.get("b").is_none());
        assert_eq!(c.len(), 2);
    }
}
