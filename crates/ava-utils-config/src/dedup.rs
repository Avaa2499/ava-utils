use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::envelope::Envelope;

pub const DEFAULT_CAPACITY: usize = 1024;
pub const DEFAULT_TTL: Duration = Duration::from_secs(600);

struct Inner {
    replies: HashMap<String, (Instant, Envelope)>,
    order: VecDeque<String>,
}

pub struct ReplyCache {
    capacity: usize,
    ttl: Duration,
    inner: Mutex<Inner>,
}

impl ReplyCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            inner: Mutex::new(Inner {
                replies: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Envelope> {
        let mut g = self.lock();
        match g.replies.get(id) {
            Some((at, reply)) if at.elapsed() <= self.ttl => Some(reply.clone()),
            Some(_) => {
                g.replies.remove(id);
                g.order.retain(|k| k != id);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, id: &str, reply: &Envelope) {
        let mut g = self.lock();
        if g.replies
            .insert(id.to_string(), (Instant::now(), reply.clone()))
            .is_none()
        {
            g.order.push_back(id.to_string());
        }
        while g.order.len() > self.capacity {
            if let Some(old) = g.order.pop_front() {
                g.replies.remove(&old);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.lock().replies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ReplyCache {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(n: u32) -> Envelope {
        let call = Envelope::call("x", &()).unwrap();
        Envelope::reply(&call, &n).unwrap()
    }

    #[test]
    fn a_stored_reply_comes_back_for_the_same_id_only() {
        let c = ReplyCache::default();
        assert!(c.get("a").is_none());
        let r = reply(1);
        c.put("a", &r);
        assert_eq!(c.get("a"), Some(r));
        assert!(c.get("b").is_none());
    }

    #[test]
    fn the_oldest_replies_are_dropped_past_the_capacity() {
        let c = ReplyCache::new(3, DEFAULT_TTL);
        for i in 0..5 {
            c.put(&format!("id{i}"), &reply(i));
        }
        assert_eq!(c.len(), 3);
        assert!(c.get("id0").is_none() && c.get("id1").is_none());
        assert!(c.get("id2").is_some() && c.get("id4").is_some());
    }

    #[test]
    fn replies_expire_after_the_ttl() {
        let c = ReplyCache::new(10, Duration::from_millis(30));
        c.put("a", &reply(1));
        std::thread::sleep(Duration::from_millis(80));
        assert!(c.get("a").is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn storing_the_same_id_again_does_not_double_count_it() {
        let c = ReplyCache::new(2, DEFAULT_TTL);
        c.put("a", &reply(1));
        c.put("a", &reply(2));
        c.put("b", &reply(3));
        assert_eq!(c.len(), 2);
        assert!(c.get("a").is_some());
    }
}
