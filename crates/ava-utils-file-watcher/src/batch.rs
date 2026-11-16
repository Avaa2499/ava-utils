use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ava_utils_config::messages::FilesChanged;

use crate::limit::Limit;

pub const ENTRY_OVERHEAD: u64 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Changed(String),
    Removed(String),
}

#[derive(Debug, Default)]
pub struct Batcher {
    changed: BTreeSet<String>,
    removed: BTreeSet<String>,
}

impl Batcher {
    pub fn push(&mut self, c: Change) {
        match c {
            Change::Changed(p) => {
                self.removed.remove(&p);
                self.changed.insert(p);
            }
            Change::Removed(p) => {
                self.changed.remove(&p);
                self.removed.insert(p);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn take(&mut self) -> FilesChanged {
        FilesChanged {
            changed: std::mem::take(&mut self.changed).into_iter().collect(),
            removed: std::mem::take(&mut self.removed).into_iter().collect(),
        }
    }
}

pub fn pump(
    rx: &Receiver<Change>,
    quiet: Duration,
    max_wait: Duration,
    mut sink: impl FnMut(FilesChanged),
) {
    let mut batcher = Batcher::default();
    while let Ok(first) = rx.recv() {
        batcher.push(first);
        let started = Instant::now();
        loop {
            let left = max_wait.saturating_sub(started.elapsed()).min(quiet);
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(c) => batcher.push(c),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    sink(batcher.take());
                    return;
                }
            }
        }
        sink(batcher.take());
    }
}

pub struct Journal {
    limit: Limit,
    bytes: u64,
    ops: VecDeque<(String, bool)>,
}

impl Journal {
    pub fn new(limit: Limit) -> Self {
        Self {
            limit,
            bytes: 0,
            ops: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn limit(&self) -> Limit {
        self.limit
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn set_limit(&mut self, limit: Limit) {
        self.limit = limit;
        self.evict();
    }

    fn add(&mut self, path: &str, removed: bool) {
        self.bytes += path.len() as u64 + ENTRY_OVERHEAD;
        self.ops.push_back((path.to_string(), removed));
    }

    fn evict(&mut self) {
        loop {
            let over = match self.limit {
                Limit::Count(n) => self.ops.len() > n,
                Limit::Bytes(b) => self.bytes > b,
            };
            if !over {
                return;
            }
            match self.ops.pop_front() {
                Some((p, _)) => self.bytes -= p.len() as u64 + ENTRY_OVERHEAD,
                None => return,
            }
        }
    }

    pub fn record(&mut self, batch: &FilesChanged) {
        for p in &batch.removed {
            self.add(p, true);
        }
        for p in &batch.changed {
            self.add(p, false);
        }
        self.evict();
    }

    pub fn state(&self) -> FilesChanged {
        let mut last: BTreeMap<&str, bool> = BTreeMap::new();
        for (p, removed) in &self.ops {
            last.insert(p.as_str(), *removed);
        }
        let mut out = FilesChanged::default();
        for (p, removed) in last {
            if removed {
                out.removed.push(p.to_string());
            } else {
                out.changed.push(p.to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::thread;

    fn c(p: &str) -> Change {
        Change::Changed(p.into())
    }

    fn r(p: &str) -> Change {
        Change::Removed(p.into())
    }

    #[test]
    fn the_last_thing_that_happened_to_a_path_wins() {
        let mut b = Batcher::default();
        for ch in [c("/a"), r("/a"), r("/b"), c("/b"), c("/c"), c("/c")] {
            b.push(ch);
        }
        let out = b.take();
        assert_eq!(out.changed, ["/b", "/c"]);
        assert_eq!(out.removed, ["/a"]);
        assert!(b.is_empty());
    }

    #[test]
    fn a_burst_becomes_one_batch_and_a_later_event_a_second() {
        let (tx, rx) = channel();
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let o = out.clone();
        let h = thread::spawn(move || {
            pump(
                &rx,
                Duration::from_millis(80),
                Duration::from_secs(2),
                |b| o.lock().unwrap().push(b),
            )
        });
        for i in 0..50 {
            tx.send(c(&format!("/f{i}"))).unwrap();
        }
        thread::sleep(Duration::from_millis(400));
        tx.send(r("/gone")).unwrap();
        drop(tx);
        h.join().unwrap();
        let got = out.lock().unwrap();
        assert!(got.len() >= 2, "the late event must be its own batch");
        let all: std::collections::BTreeSet<&String> =
            got.iter().flat_map(|b| &b.changed).collect();
        assert_eq!(
            all.len(),
            50,
            "no change may be lost, however the burst was split"
        );
        let last = got.last().unwrap();
        assert_eq!(last.removed, ["/gone"]);
        assert!(last.changed.is_empty());
    }

    #[test]
    fn a_constant_stream_still_flushes_by_the_max_wait() {
        let (tx, rx) = channel();
        let out = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let o = out.clone();
        let h = thread::spawn(move || {
            pump(
                &rx,
                Duration::from_millis(100),
                Duration::from_millis(300),
                |_| *o.lock().unwrap() += 1,
            )
        });
        for i in 0..40 {
            tx.send(c(&format!("/f{i}"))).unwrap();
            thread::sleep(Duration::from_millis(20));
        }
        drop(tx);
        h.join().unwrap();
        assert!(*out.lock().unwrap() >= 2);
    }

    #[test]
    fn the_journal_keeps_the_latest_state_and_drops_the_oldest() {
        let mut j = Journal::new(Limit::Count(3));
        j.record(&FilesChanged {
            changed: vec!["/a".into(), "/b".into()],
            removed: vec![],
        });
        j.record(&FilesChanged {
            changed: vec![],
            removed: vec!["/a".into()],
        });
        let s = j.state();
        assert_eq!(s.removed, ["/a"]);
        assert_eq!(s.changed, ["/b"]);
        j.record(&FilesChanged {
            changed: vec!["/c".into(), "/d".into()],
            removed: vec![],
        });
        assert_eq!(j.state().changed, ["/c", "/d"]);
    }

    #[test]
    fn a_memory_limit_evicts_by_estimated_size_and_can_change_live() {
        let one = 2 + ENTRY_OVERHEAD;
        let mut j = Journal::new(Limit::Bytes(one * 3));
        let batch = FilesChanged {
            changed: (0..10).map(|i| format!("/{i}")).collect(),
            removed: vec![],
        };
        j.record(&batch);
        assert_eq!(j.len(), 3);
        assert_eq!(j.bytes(), one * 3);
        assert_eq!(j.state().changed, ["/7", "/8", "/9"]);
        j.set_limit(Limit::Count(1));
        assert_eq!((j.len(), j.bytes()), (1, one));
        j.set_limit(Limit::Count(100));
        j.record(&batch);
        assert_eq!(j.len(), 11);
    }
}
