use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ava_utils_config::messages::FilesChanged;

use crate::limit::Limit;

pub const ENTRY_OVERHEAD: u64 = 64;
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;
const WIRE_OVERHEAD: usize = 8;

pub fn split(batch: &FilesChanged, limit: usize) -> Vec<FilesChanged> {
    let size = |p: &String| p.len() + WIRE_OVERHEAD;
    let mut out = Vec::new();
    let mut cur = FilesChanged::default();
    let mut bytes = 0;
    let removed = batch.removed.iter().map(|p| (p, true));
    let changed = batch.changed.iter().map(|p| (p, false));
    for (p, gone) in removed.chain(changed) {
        if bytes > 0 && bytes + size(p) > limit {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += size(p);
        if gone {
            cur.removed.push(p.clone());
        } else {
            cur.changed.push(p.clone());
        }
    }
    if bytes > 0 || out.is_empty() {
        out.push(cur);
    }
    out
}

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
            ..Default::default()
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
    ops: VecDeque<(String, bool, u64)>,
    epoch: u64,
    seq: u64,
    evicted_through: u64,
    roots: Vec<String>,
    chunk: usize,
}

impl Journal {
    pub fn new(limit: Limit) -> Self {
        Self::with_epoch(limit, ava_utils_config::id::new_epoch().unwrap_or(1))
    }

    pub fn with_epoch(limit: Limit, epoch: u64) -> Self {
        Self {
            limit,
            bytes: 0,
            ops: VecDeque::new(),
            epoch,
            seq: 0,
            evicted_through: 0,
            roots: Vec::new(),
            chunk: CHUNK_BYTES,
        }
    }

    pub fn with_chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk;
        self
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn first_seq(&self) -> u64 {
        self.evicted_through + 1
    }

    pub fn set_roots(&mut self, roots: Vec<String>) {
        self.roots = roots;
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

    fn add(&mut self, path: &str, removed: bool, seq: u64) {
        self.bytes += path.len() as u64 + ENTRY_OVERHEAD;
        self.ops.push_back((path.to_string(), removed, seq));
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
                Some((p, _, seq)) => {
                    self.bytes -= p.len() as u64 + ENTRY_OVERHEAD;
                    self.evicted_through = self.evicted_through.max(seq);
                }
                None => return,
            }
        }
    }

    pub fn record(&mut self, batch: &FilesChanged) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        for p in &batch.removed {
            self.add(p, true, seq);
        }
        for p in &batch.changed {
            self.add(p, false, seq);
        }
        self.evict();
        seq
    }

    pub fn state(&self) -> FilesChanged {
        let mut last: BTreeMap<&str, bool> = BTreeMap::new();
        for (p, removed, _) in &self.ops {
            last.insert(p.as_str(), *removed);
        }
        let mut out = FilesChanged {
            epoch: self.epoch,
            seq: self.seq,
            first_seq: self.first_seq(),
            roots: self.roots.clone(),
            ..Default::default()
        };
        let mut bytes = 0;
        for (p, removed) in last {
            bytes += p.len() + WIRE_OVERHEAD;
            if bytes > self.chunk {
                out.first_seq = self.seq + 1;
                break;
            }
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
        let t = std::time::Instant::now();
        while out.lock().unwrap().is_empty() {
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "the burst never flushed"
            );
            thread::sleep(Duration::from_millis(5));
        }
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
            ..Default::default()
        });
        j.record(&FilesChanged {
            changed: vec![],
            removed: vec!["/a".into()],
            ..Default::default()
        });
        let s = j.state();
        assert_eq!(s.removed, ["/a"]);
        assert_eq!(s.changed, ["/b"]);
        j.record(&FilesChanged {
            changed: vec!["/c".into(), "/d".into()],
            removed: vec![],
            ..Default::default()
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
            ..Default::default()
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

    fn batch_of(paths: &[&str]) -> FilesChanged {
        FilesChanged {
            changed: paths.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_journal_numbers_each_batch_and_starts_with_everything_covered() {
        let mut j = Journal::with_epoch(Limit::Count(100), 9);
        assert_eq!((j.epoch(), j.seq(), j.first_seq()), (9, 0, 1));
        assert_eq!(j.record(&batch_of(&["/a", "/b"])), 1);
        assert_eq!(j.record(&batch_of(&["/c"])), 2);
        assert_eq!((j.seq(), j.first_seq()), (2, 1));
        let s = j.state();
        assert_eq!((s.epoch, s.seq, s.first_seq), (9, 2, 1));
    }

    #[test]
    fn evicting_part_of_a_batch_means_that_batch_is_no_longer_fully_covered() {
        let mut j = Journal::with_epoch(Limit::Count(3), 1);
        j.record(&batch_of(&["/a", "/b"]));
        assert_eq!(j.first_seq(), 1);
        j.record(&batch_of(&["/c", "/d"]));
        assert_eq!(j.len(), 3);
        assert_eq!(
            j.first_seq(),
            2,
            "batch 1 lost an entry, so only batches after it are complete"
        );
        j.record(&batch_of(&["/e", "/f"]));
        assert_eq!(j.first_seq(), 3);
    }

    #[test]
    fn a_single_batch_bigger_than_the_limit_is_never_fully_covered() {
        let mut j = Journal::with_epoch(Limit::Count(2), 1);
        let seq = j.record(&batch_of(&["/a", "/b", "/c", "/d"]));
        assert_eq!(seq, 1);
        assert_eq!(j.first_seq(), 2);
        assert!(
            j.first_seq() > j.seq(),
            "even a subscriber that has seen nothing cannot be caught up from the journal"
        );
    }

    #[test]
    fn shrinking_the_limit_moves_first_seq_forward() {
        let mut j = Journal::with_epoch(Limit::Count(100), 1);
        j.record(&batch_of(&["/a"]));
        j.record(&batch_of(&["/b"]));
        j.record(&batch_of(&["/c"]));
        j.set_limit(Limit::Count(1));
        assert_eq!(j.first_seq(), 3);
        j.set_limit(Limit::Count(100));
        assert_eq!(
            j.first_seq(),
            3,
            "a bigger limit cannot bring evicted entries back"
        );
    }

    #[test]
    fn the_roots_are_part_of_the_state() {
        let mut j = Journal::with_epoch(Limit::Count(10), 1);
        assert!(j.state().roots.is_empty());
        j.set_roots(vec!["/r".into()]);
        assert_eq!(j.state().roots, ["/r"]);
    }

    #[test]
    fn a_fresh_journal_picks_a_random_epoch() {
        let a = Journal::new(Limit::Count(1));
        let b = Journal::new(Limit::Count(1));
        assert!(a.epoch() != 0 && a.epoch() != b.epoch());
    }

    fn many(n: usize, len: usize) -> FilesChanged {
        FilesChanged {
            changed: (0..n).map(|i| format!("{i:0>len$}")).collect(),
            removed: (0..n / 10).map(|i| format!("r{i:0>len$}")).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_small_batch_is_not_split() {
        let b = many(10, 20);
        assert_eq!(split(&b, CHUNK_BYTES), [b]);
    }

    #[test]
    fn an_empty_batch_stays_one_empty_batch() {
        assert_eq!(split(&FilesChanged::default(), 100).len(), 1);
    }

    #[test]
    fn a_big_batch_splits_into_pieces_that_together_hold_every_path_once() {
        let b = many(1000, 50);
        let parts = split(&b, 4000);
        assert!(parts.len() > 10);
        let mut changed: Vec<String> = parts.iter().flat_map(|p| p.changed.clone()).collect();
        let mut removed: Vec<String> = parts.iter().flat_map(|p| p.removed.clone()).collect();
        changed.sort();
        removed.sort();
        let (mut ec, mut er) = (b.changed.clone(), b.removed.clone());
        ec.sort();
        er.sort();
        assert_eq!((changed, removed), (ec, er));
        for p in &parts {
            let bytes: usize = p
                .changed
                .iter()
                .chain(&p.removed)
                .map(|x| x.len() + 8)
                .sum();
            assert!(bytes <= 4000, "{bytes}");
        }
    }

    #[test]
    fn a_batch_over_the_frame_limit_arrives_as_frames_that_each_fit() {
        use ava_utils_config::envelope::Envelope;
        use ava_utils_config::frame::MAX_FRAME_BYTES;
        let b = many(200_000, 100);
        let parts = split(&b, CHUNK_BYTES);
        assert!(parts.len() >= 2);
        for p in &parts {
            let len = Envelope::event("files-changed", p)
                .unwrap()
                .encode()
                .unwrap()
                .len();
            assert!(len < MAX_FRAME_BYTES as usize, "{len}");
        }
        let total: usize = parts
            .iter()
            .map(|p| p.changed.len() + p.removed.len())
            .sum();
        assert_eq!(total, 220_000);
    }

    #[test]
    fn each_piece_gets_its_own_sequence_number() {
        let mut j = Journal::with_epoch(Limit::Count(1_000_000), 5);
        let seqs: Vec<u64> = split(&many(100, 50), 1000)
            .iter()
            .map(|p| j.record(p))
            .collect();
        assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
        assert!(seqs.len() > 3);
    }

    #[test]
    fn a_state_too_big_for_one_frame_is_trimmed_and_tells_readers_to_rescan() {
        let mut j = Journal::with_epoch(Limit::Count(1_000_000), 5).with_chunk(2000);
        j.record(&many(500, 50));
        let s = j.state();
        assert!(s.changed.len() + s.removed.len() < 500);
        assert_eq!(
            s.first_seq,
            s.seq + 1,
            "the journal no longer covers a reader's gap"
        );
        assert_eq!(s.epoch, 5);
    }

    #[test]
    fn a_state_that_fits_is_complete_and_keeps_its_real_first_seq() {
        let mut j = Journal::with_epoch(Limit::Count(1_000_000), 5);
        j.record(&many(100, 20));
        let s = j.state();
        assert_eq!(s.changed.len(), 100);
        assert_eq!(s.first_seq, 1);
    }
}
