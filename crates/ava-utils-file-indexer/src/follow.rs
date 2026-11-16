use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use ava_utils_config::error::Result;
use ava_utils_config::messages::{FilesChanged, JobState};

use crate::db::Index;
use crate::service::Service;

const EPOCH_KEY: &str = "watch-epoch";
const SEQ_KEY: &str = "watch-seq";

#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Covered,
    NeedsScan(Vec<String>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Apply,
    Skip,
    Gap,
}

pub struct Tracker {
    index: Arc<Index>,
    epoch: u64,
    last_seq: u64,
    clean: bool,
    session: u64,
}

impl Tracker {
    pub fn new(index: Arc<Index>) -> Self {
        Self {
            index,
            epoch: 0,
            last_seq: 0,
            clean: false,
            session: 0,
        }
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    pub fn begin(&mut self, state: &FilesChanged) -> Result<Start> {
        self.session += 1;
        let cursor_epoch = self.index.meta(EPOCH_KEY)?.unwrap_or(0);
        let cursor_seq = self.index.meta(SEQ_KEY)?.unwrap_or(0);
        self.epoch = state.epoch;
        self.last_seq = state.seq;
        let covered = state.epoch != 0
            && cursor_epoch == state.epoch
            && cursor_seq <= state.seq
            && state.first_seq <= cursor_seq + 1;
        if covered {
            self.clean = true;
            return Ok(Start::Covered);
        }
        if state.roots.is_empty() {
            self.clean = true;
            return Ok(Start::Covered);
        }
        self.clean = false;
        Ok(Start::NeedsScan(state.roots.clone()))
    }

    pub fn event(&mut self, ev: &FilesChanged) -> Next {
        if ev.epoch == 0 || ev.epoch != self.epoch {
            return Next::Gap;
        }
        if ev.seq <= self.last_seq {
            return Next::Skip;
        }
        if ev.seq == self.last_seq + 1 {
            self.last_seq = ev.seq;
            return Next::Apply;
        }
        Next::Gap
    }

    pub fn mark_applied(&self) -> Result<()> {
        if self.clean && self.epoch != 0 {
            self.index.set_meta(EPOCH_KEY, self.epoch)?;
            self.index.set_meta(SEQ_KEY, self.last_seq)?;
        }
        Ok(())
    }

    pub fn caught_up(&mut self, session: u64) -> Result<bool> {
        if session != self.session {
            return Ok(false);
        }
        self.clean = true;
        self.mark_applied()?;
        Ok(true)
    }
}

fn lock(t: &Mutex<Tracker>) -> MutexGuard<'_, Tracker> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

pub const POLL: Duration = Duration::from_millis(300);

pub fn catch_up(
    service: Arc<Service>,
    tracker: Arc<Mutex<Tracker>>,
    session: u64,
    roots: Vec<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let roots: Vec<String> = roots
            .into_iter()
            .filter(|r| std::path::Path::new(r).exists())
            .collect();
        if roots.is_empty() {
            let _ = lock(&tracker).caught_up(session);
            return;
        }
        let id = loop {
            if lock(&tracker).session() != session {
                return;
            }
            match service.jobs.start_scan(roots.clone()) {
                Ok(id) => break id,
                Err(e) if e.to_string().contains("already running") => thread::sleep(POLL),
                Err(e) => {
                    eprintln!("file-indexer: could not start the catch-up scan: {e}");
                    return;
                }
            }
        };
        loop {
            match service.jobs.status(&id) {
                Ok(s) if s.state == JobState::Running => thread::sleep(POLL),
                Ok(s) if s.state == JobState::Done => {
                    if let Err(e) = lock(&tracker).caught_up(session) {
                        eprintln!("file-indexer: could not save the watcher position: {e}");
                    }
                    return;
                }
                Ok(s) => {
                    eprintln!(
                        "file-indexer: catch-up scan {id} ended as {:?}, will retry on the next connect",
                        s.state
                    );
                    return;
                }
                Err(e) => {
                    eprintln!("file-indexer: lost the catch-up scan {id}: {e}");
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{temp_index, test_tree_root};
    use std::time::Instant;

    fn state(epoch: u64, seq: u64, first_seq: u64, roots: &[&str]) -> FilesChanged {
        FilesChanged {
            epoch,
            seq,
            first_seq,
            roots: roots.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }
    }

    fn event(epoch: u64, seq: u64) -> FilesChanged {
        FilesChanged {
            epoch,
            seq,
            changed: vec!["/x".into()],
            ..Default::default()
        }
    }

    fn tracker(tag: &str) -> (std::path::PathBuf, Arc<Index>, Tracker) {
        let (dir, ix) = temp_index(tag);
        let ix = Arc::new(ix);
        (dir, ix.clone(), Tracker::new(ix))
    }

    fn cursor(ix: &Index) -> (u64, u64) {
        (
            ix.meta(EPOCH_KEY).unwrap().unwrap_or(0),
            ix.meta(SEQ_KEY).unwrap().unwrap_or(0),
        )
    }

    fn scan_of(roots: &[&str]) -> Start {
        Start::NeedsScan(roots.iter().map(|r| r.to_string()).collect())
    }

    #[test]
    fn the_very_first_connection_needs_a_scan_of_the_watched_roots() {
        let (d, _ix, mut t) = tracker("fo-first");
        assert_eq!(t.begin(&state(5, 3, 1, &["/r"])).unwrap(), scan_of(&["/r"]));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn nothing_is_saved_until_the_scan_finishes() {
        let (d, ix, mut t) = tracker("fo-dirty");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.mark_applied().unwrap();
        assert_eq!(t.event(&event(5, 4)), Next::Apply);
        t.mark_applied().unwrap();
        assert_eq!(
            cursor(&ix),
            (0, 0),
            "a pending catch-up must not move the saved position"
        );
        assert!(t.caught_up(t.session()).unwrap());
        assert_eq!(cursor(&ix), (5, 4));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_stale_session_cannot_mark_the_index_caught_up() {
        let (d, ix, mut t) = tracker("fo-stale");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        let old = t.session();
        t.begin(&state(6, 1, 1, &["/r"])).unwrap();
        assert!(!t.caught_up(old).unwrap());
        assert_eq!(cursor(&ix), (0, 0));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reconnecting_to_the_same_watcher_with_a_covering_journal_needs_no_scan() {
        let (d, ix, mut t) = tracker("fo-covered");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.begin(&state(5, 9, 1, &["/r"])).unwrap(), Start::Covered);
        t.mark_applied().unwrap();
        assert_eq!(cursor(&ix), (5, 9));
        assert_eq!(t.begin(&state(5, 9, 9, &["/r"])).unwrap(), Start::Covered);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_journal_that_dropped_changes_we_never_saw_needs_a_scan() {
        let (d, _ix, mut t) = tracker("fo-evicted");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(
            t.begin(&state(5, 50, 5, &["/r"])).unwrap(),
            scan_of(&["/r"])
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn exactly_the_next_batch_still_being_held_is_covered() {
        let (d, _ix, mut t) = tracker("fo-edge");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.begin(&state(5, 10, 4, &["/r"])).unwrap(), Start::Covered);
        assert_eq!(
            t.begin(&state(5, 10, 5, &["/r"])).unwrap(),
            scan_of(&["/r"])
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_restarted_watcher_means_a_scan_because_changes_while_it_was_down_were_never_seen() {
        let (d, _ix, mut t) = tracker("fo-epoch");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.begin(&state(6, 0, 1, &["/r"])).unwrap(), scan_of(&["/r"]));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_watcher_that_went_backwards_in_sequence_is_not_trusted() {
        let (d, _ix, mut t) = tracker("fo-back");
        t.begin(&state(5, 30, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.begin(&state(5, 4, 1, &["/r"])).unwrap(), scan_of(&["/r"]));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_old_watcher_without_an_epoch_always_gets_a_scan() {
        let (d, _ix, mut t) = tracker("fo-old");
        assert_eq!(t.begin(&state(0, 0, 0, &["/r"])).unwrap(), scan_of(&["/r"]));
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.begin(&state(0, 0, 0, &["/r"])).unwrap(), scan_of(&["/r"]));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn with_nothing_watched_there_is_nothing_to_scan() {
        let (d, ix, mut t) = tracker("fo-noroots");
        assert_eq!(t.begin(&state(5, 3, 1, &[])).unwrap(), Start::Covered);
        t.mark_applied().unwrap();
        assert_eq!(cursor(&ix), (5, 3));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_saved_position_survives_a_restart_of_the_indexer() {
        let (d, ix, mut t) = tracker("fo-restart");
        t.begin(&state(5, 3, 1, &["/r"])).unwrap();
        t.caught_up(t.session()).unwrap();
        assert_eq!(t.event(&event(5, 4)), Next::Apply);
        t.mark_applied().unwrap();
        let mut again = Tracker::new(ix);
        assert_eq!(
            again.begin(&state(5, 4, 1, &["/r"])).unwrap(),
            Start::Covered
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn events_must_arrive_in_order_duplicates_are_skipped_and_a_jump_is_a_gap() {
        let (d, _ix, mut t) = tracker("fo-events");
        t.begin(&state(5, 10, 1, &["/r"])).unwrap();
        assert_eq!(
            t.event(&event(5, 10)),
            Next::Skip,
            "already inside the state we were sent"
        );
        assert_eq!(t.event(&event(5, 9)), Next::Skip);
        assert_eq!(t.event(&event(5, 11)), Next::Apply);
        assert_eq!(t.event(&event(5, 11)), Next::Skip);
        assert_eq!(t.event(&event(5, 13)), Next::Gap, "12 never arrived");
        assert_eq!(t.event(&event(6, 12)), Next::Gap, "a different watcher run");
        assert_eq!(t.event(&event(0, 12)), Next::Gap, "no epoch at all");
        let _ = std::fs::remove_dir_all(d);
    }

    fn service(tag: &str) -> (std::path::PathBuf, Arc<Service>, std::path::PathBuf) {
        let (dir, ix) = temp_index(tag);
        drop(ix);
        let root = test_tree_root(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/missed.txt"), b"hi").unwrap();
        let s = Arc::new(Service::open(&dir.join("index.redb"), 2).unwrap());
        (dir, s, root)
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn shared(s: &Arc<Service>) -> Arc<Mutex<Tracker>> {
        Arc::new(Mutex::new(Tracker::new(s.index.clone())))
    }

    #[test]
    fn a_catch_up_scan_finds_files_changed_while_nobody_was_watching() {
        let (d, s, root) = service("fo-catch");
        let root_s = root.to_string_lossy().into_owned();
        let t = shared(&s);
        let session = {
            let mut g = lock(&t);
            assert_eq!(
                g.begin(&state(5, 0, 1, &[&root_s])).unwrap(),
                scan_of(&[&root_s])
            );
            g.session()
        };
        assert!(
            s.index
                .get(&format!("{root_s}/sub/missed.txt"))
                .unwrap()
                .is_none()
        );
        catch_up(s.clone(), t.clone(), session, vec![root_s.clone()]);
        wait_for(|| cursor(&s.index).0 == 5);
        assert!(
            s.index
                .get(&format!("{root_s}/sub/missed.txt"))
                .unwrap()
                .is_some()
        );
        let _ = (std::fs::remove_dir_all(d), std::fs::remove_dir_all(root));
    }

    #[test]
    fn a_catch_up_waits_for_a_scan_already_running_and_then_still_runs() {
        let (d, s, root) = service("fo-busy");
        let root_s = root.to_string_lossy().into_owned();
        s.jobs.start_scan(vec![root_s.clone()]).unwrap();
        let t = shared(&s);
        let session = {
            let mut g = lock(&t);
            g.begin(&state(7, 0, 1, &[&root_s])).unwrap();
            g.session()
        };
        catch_up(s.clone(), t.clone(), session, vec![root_s.clone()]);
        wait_for(|| cursor(&s.index).0 == 7);
        let _ = (std::fs::remove_dir_all(d), std::fs::remove_dir_all(root));
    }

    #[test]
    fn roots_that_no_longer_exist_are_skipped_and_nothing_is_left_pending() {
        let (d, s, root) = service("fo-gone");
        let t = shared(&s);
        let session = {
            let mut g = lock(&t);
            g.begin(&state(8, 0, 1, &["/definitely/not/here"])).unwrap();
            g.session()
        };
        catch_up(
            s.clone(),
            t.clone(),
            session,
            vec!["/definitely/not/here".into()],
        );
        wait_for(|| cursor(&s.index).0 == 8);
        let _ = (std::fs::remove_dir_all(d), std::fs::remove_dir_all(root));
    }

    #[test]
    fn a_catch_up_for_an_old_session_does_not_save_anything() {
        let (d, s, root) = service("fo-oldsession");
        let root_s = root.to_string_lossy().into_owned();
        let t = shared(&s);
        let old = {
            let mut g = lock(&t);
            g.begin(&state(9, 0, 1, &[&root_s])).unwrap();
            g.session()
        };
        lock(&t).begin(&state(10, 0, 1, &[])).unwrap();
        catch_up(s.clone(), t.clone(), old, vec![root_s])
            .join()
            .unwrap();
        assert_eq!(cursor(&s.index).0, 0);
        let _ = (std::fs::remove_dir_all(d), std::fs::remove_dir_all(root));
    }
}
