use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use ava_utils_config::error::Result;

use crate::apply::entry_from_meta;
use crate::db::{Entry, Index};
use crate::exclude::{is_excluded, is_virtual};

pub const RUN: u8 = 0;
pub const FORCE: u8 = 1;
pub const SAFE: u8 = 2;
pub const BATCH_ENTRIES: usize = 50_000;

pub struct ScanControl {
    pub cancel: AtomicU8,
    pub entries: AtomicU64,
    pub dirs: AtomicU64,
    pub started: Instant,
    pub pause_per_dir: Duration,
}

impl ScanControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicU8::new(RUN),
            entries: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            started: Instant::now(),
            pause_per_dir: Duration::ZERO,
        })
    }

    pub fn slow(pause_per_dir: Duration) -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicU8::new(RUN),
            entries: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            started: Instant::now(),
            pause_per_dir,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEnd {
    Completed,
    Paused,
    Cancelled,
}

struct Shared {
    queue: VecDeque<String>,
    active: usize,
    uncommitted: HashSet<String>,
}

struct Walk {
    dir: String,
    entries: Vec<Entry>,
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn walk_level(dir: &str, exclusions: &[String], generation: u64) -> (Walk, Vec<String>) {
    let mut entries = Vec::new();
    let mut subdirs = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            let Some(ps) = path.to_str() else { continue };
            if is_virtual(&path) {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if is_excluded(ps, meta.is_dir(), exclusions) {
                continue;
            }
            if meta.is_dir() {
                subdirs.push(ps.to_string());
            }
            entries.push(entry_from_meta(ps, &meta, generation));
        }
    }
    (
        Walk {
            dir: dir.to_string(),
            entries,
        },
        subdirs,
    )
}

pub fn run_scan(
    index: &Arc<Index>,
    job: &str,
    roots: &[String],
    start: Vec<String>,
    generation: u64,
    ctl: &Arc<ScanControl>,
    workers: usize,
) -> Result<ScanEnd> {
    let exclusions = Arc::new(index.exclusions()?);
    let mut first = Vec::new();
    for root in roots {
        if let Ok(meta) = std::fs::symlink_metadata(root) {
            first.push(entry_from_meta(root, &meta, generation));
        }
    }
    let shared = Arc::new((
        Mutex::new(Shared {
            queue: start.into_iter().collect(),
            active: 0,
            uncommitted: HashSet::new(),
        }),
        Condvar::new(),
    ));
    let frontier_now = |batch: &[String]| -> Vec<String> {
        let g = lock(&shared.0);
        let mut out: Vec<String> = g.queue.iter().cloned().collect();
        out.extend(g.uncommitted.iter().filter(|d| !batch.contains(d)).cloned());
        out
    };
    let initial = frontier_now(&[]);
    index.commit_scan(job, &first, &initial)?;

    let (tx, rx) = mpsc::channel::<Walk>();
    let mut handles = Vec::new();
    for _ in 0..workers.max(1) {
        let shared = shared.clone();
        let tx = tx.clone();
        let ctl = ctl.clone();
        let exclusions = exclusions.clone();
        handles.push(thread::spawn(move || {
            loop {
                let dir = {
                    let mut g = lock(&shared.0);
                    loop {
                        if ctl.cancel.load(Ordering::SeqCst) != RUN {
                            return;
                        }
                        if let Some(d) = g.queue.pop_front() {
                            g.active += 1;
                            g.uncommitted.insert(d.clone());
                            break d;
                        }
                        if g.active == 0 {
                            shared.1.notify_all();
                            return;
                        }
                        g = shared
                            .1
                            .wait_timeout(g, Duration::from_millis(50))
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                };
                if !ctl.pause_per_dir.is_zero() {
                    thread::sleep(ctl.pause_per_dir);
                }
                let (walk, subdirs) = walk_level(&dir, &exclusions, generation);
                {
                    let mut g = lock(&shared.0);
                    g.queue.extend(subdirs);
                    g.active -= 1;
                    shared.1.notify_all();
                }
                if tx.send(walk).is_err() {
                    return;
                }
            }
        }));
    }
    drop(tx);

    let mut batch: Vec<Entry> = Vec::new();
    let mut batch_dirs: Vec<String> = Vec::new();
    let mut failure = None;
    for walk in &rx {
        if ctl.cancel.load(Ordering::SeqCst) == FORCE {
            break;
        }
        ctl.dirs.fetch_add(1, Ordering::SeqCst);
        batch.extend(walk.entries);
        batch_dirs.push(walk.dir);
        if batch.len() >= BATCH_ENTRIES
            && let Err(e) = commit(index, job, &mut batch, &mut batch_dirs, &frontier_now, ctl)
        {
            failure = Some(e);
            ctl.cancel.store(FORCE, Ordering::SeqCst);
            break;
        }
    }
    let forced = ctl.cancel.load(Ordering::SeqCst) == FORCE;
    if !forced
        && failure.is_none()
        && let Err(e) = commit(index, job, &mut batch, &mut batch_dirs, &frontier_now, ctl)
    {
        failure = Some(e);
    }
    drop(rx);
    for h in handles {
        let _ = h.join();
    }
    if let Some(e) = failure {
        return Err(e);
    }
    if forced {
        index.clear_frontier(job)?;
        return Ok(ScanEnd::Cancelled);
    }
    if !lock(&shared.0).queue.is_empty() {
        return Ok(ScanEnd::Paused);
    }
    index.clear_frontier(job)?;
    index.finalize_scan(roots, generation)?;
    Ok(ScanEnd::Completed)
}

fn commit(
    index: &Index,
    job: &str,
    batch: &mut Vec<Entry>,
    batch_dirs: &mut Vec<String>,
    frontier_now: &dyn Fn(&[String]) -> Vec<String>,
    ctl: &ScanControl,
) -> Result<()> {
    let frontier = frontier_now(batch_dirs);
    index.commit_scan(job, batch, &frontier)?;
    ctl.entries.fetch_add(batch.len() as u64, Ordering::SeqCst);
    batch.clear();
    batch_dirs.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{temp_index, test_tree_root};

    fn build_tree(tag: &str, fanout: usize, depth: usize) -> std::path::PathBuf {
        let root = test_tree_root(tag);
        let _ = std::fs::remove_dir_all(&root);
        fn grow(dir: &std::path::Path, fanout: usize, depth: usize) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("f.txt"), b"12345").unwrap();
            std::fs::write(dir.join("g.rs"), b"1").unwrap();
            if depth > 0 {
                for i in 0..fanout {
                    grow(&dir.join(format!("d{i}")), fanout, depth - 1);
                }
            }
        }
        grow(&root, fanout, depth);
        root
    }

    fn s(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn expected_entries(fanout: usize, depth: usize) -> u64 {
        let dirs: u64 = (0..=depth).map(|d| (fanout as u64).pow(d as u32)).sum();
        dirs * 2 + dirs
    }

    fn scan_all(ix: &Arc<Index>, root: &str, gen_: u64, workers: usize) -> ScanEnd {
        run_scan(
            ix,
            "job",
            &[root.to_string()],
            vec![root.to_string()],
            gen_,
            &ScanControl::new(),
            workers,
        )
        .unwrap()
    }

    #[test]
    fn a_full_scan_indexes_everything_and_sizes_folders() {
        let (dir, ix) = temp_index("scan-full");
        let ix = Arc::new(ix);
        let root = build_tree("scan-full", 3, 3);
        assert_eq!(scan_all(&ix, &s(&root), 1, 4), ScanEnd::Completed);
        assert_eq!(ix.count().unwrap(), expected_entries(3, 3));
        let dirs: u64 = (0..=3u32).map(|d| 3u64.pow(d)).sum();
        assert_eq!(ix.folder_size(&s(&root)).unwrap(), Some(dirs * 6));
        assert!(ix.frontier("job").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn excluded_folders_are_not_entered() {
        let (dir, ix) = temp_index("scan-excl");
        let ix = Arc::new(ix);
        let root = build_tree("scan-excl", 2, 1);
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/x.js"), b"x").unwrap();
        ix.add_exclusion("node_modules/").unwrap();
        scan_all(&ix, &s(&root), 1, 2);
        assert_eq!(ix.get(&s(&root.join("node_modules"))).unwrap(), None);
        assert_eq!(
            ix.get(&s(&root.join("node_modules/pkg/x.js"))).unwrap(),
            None
        );
        assert!(ix.get(&s(&root.join("f.txt"))).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_rescan_drops_files_that_were_deleted_and_picks_up_new_ones() {
        let (dir, ix) = temp_index("scan-rescan");
        let ix = Arc::new(ix);
        let root = build_tree("scan-rescan", 2, 1);
        scan_all(&ix, &s(&root), 1, 2);
        std::fs::remove_file(root.join("f.txt")).unwrap();
        std::fs::remove_dir_all(root.join("d0")).unwrap();
        std::fs::write(root.join("new.txt"), b"n").unwrap();
        scan_all(&ix, &s(&root), 2, 2);
        assert_eq!(ix.get(&s(&root.join("f.txt"))).unwrap(), None);
        assert_eq!(ix.get(&s(&root.join("d0/f.txt"))).unwrap(), None);
        assert!(ix.get(&s(&root.join("new.txt"))).unwrap().is_some());
        assert!(ix.get(&s(&root.join("d1/f.txt"))).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_safe_cancel_saves_where_it_was_and_resume_finishes_with_no_lost_work() {
        let (dir, ix) = temp_index("scan-safe");
        let ix = Arc::new(ix);
        let root = build_tree("scan-safe", 4, 3);
        let r = s(&root);
        let ctl = ScanControl::slow(Duration::from_millis(3));
        let canceller = {
            let ctl = ctl.clone();
            thread::spawn(move || {
                while ctl.dirs.load(Ordering::SeqCst) < 20 {
                    thread::sleep(Duration::from_millis(2));
                }
                ctl.cancel.store(SAFE, Ordering::SeqCst);
            })
        };
        let end = run_scan(
            &ix,
            "job",
            std::slice::from_ref(&r),
            vec![r.clone()],
            1,
            &ctl,
            2,
        )
        .unwrap();
        canceller.join().unwrap();
        assert_eq!(end, ScanEnd::Paused);
        let saved = ix.frontier("job").unwrap();
        assert!(!saved.is_empty());
        let partial = ix.count().unwrap();
        assert!(partial < expected_entries(4, 3));
        let end = run_scan(
            &ix,
            "job",
            std::slice::from_ref(&r),
            saved,
            1,
            &ScanControl::new(),
            4,
        )
        .unwrap();
        assert_eq!(end, ScanEnd::Completed);
        assert_eq!(ix.count().unwrap(), expected_entries(4, 3));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_force_cancel_stops_without_finishing_and_clears_the_frontier() {
        let (dir, ix) = temp_index("scan-force");
        let ix = Arc::new(ix);
        let root = build_tree("scan-force", 4, 3);
        let r = s(&root);
        let ctl = ScanControl::slow(Duration::from_millis(3));
        let canceller = {
            let ctl = ctl.clone();
            thread::spawn(move || {
                while ctl.dirs.load(Ordering::SeqCst) < 10 {
                    thread::sleep(Duration::from_millis(2));
                }
                ctl.cancel.store(FORCE, Ordering::SeqCst);
            })
        };
        let t = Instant::now();
        let end = run_scan(
            &ix,
            "job",
            std::slice::from_ref(&r),
            vec![r.clone()],
            1,
            &ctl,
            2,
        )
        .unwrap();
        canceller.join().unwrap();
        assert_eq!(end, ScanEnd::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(ix.frontier("job").unwrap().is_empty());
        assert!(ix.count().unwrap() < expected_entries(4, 3));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_paused_scan_resumes_after_the_index_is_reopened() {
        let (dir, ix) = temp_index("scan-crash");
        let ix = Arc::new(ix);
        let root = build_tree("scan-crash", 3, 3);
        let r = s(&root);
        let ctl = ScanControl::slow(Duration::from_millis(2));
        let c2 = ctl.clone();
        let stopper = thread::spawn(move || {
            while c2.dirs.load(Ordering::SeqCst) < 15 {
                thread::sleep(Duration::from_millis(1));
            }
            c2.cancel.store(SAFE, Ordering::SeqCst);
        });
        run_scan(
            &ix,
            "job",
            std::slice::from_ref(&r),
            vec![r.clone()],
            1,
            &ctl,
            2,
        )
        .unwrap();
        stopper.join().unwrap();
        drop(ix);
        let ix = Arc::new(Index::open(&dir.join("index.redb")).unwrap());
        let saved = ix.frontier("job").unwrap();
        assert!(!saved.is_empty());
        run_scan(&ix, "job", &[r], saved, 1, &ScanControl::new(), 3).unwrap();
        assert_eq!(ix.count().unwrap(), expected_entries(3, 3));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }
}
