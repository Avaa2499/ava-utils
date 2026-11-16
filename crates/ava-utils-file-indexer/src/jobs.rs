use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use ava_utils_config::error::{Error, Result};
use ava_utils_config::id::new_job_id;
use ava_utils_config::messages::{CancelMode, JobState, JobStatus};
use ava_utils_config::quarantine::MAX_STRIKES;
use serde::{Deserialize, Serialize};

use crate::apply::GENERATION;
use crate::db::Index;
use crate::scan::{FORCE, SAFE, ScanControl, ScanEnd, run_scan};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub roots: Vec<String>,
    pub generation: u64,
    pub state: JobState,
    pub entries: u64,
    pub dirs: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub crashes: u32,
    #[serde(default)]
    pub error: Option<String>,
    pub updated: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Running {
    id: String,
    ctl: Arc<ScanControl>,
    handle: JoinHandle<()>,
}

pub struct Jobs {
    index: Arc<Index>,
    workers: usize,
    running: Mutex<Option<Running>>,
}

fn load(index: &Index, id: &str) -> Result<JobRecord> {
    let bytes = index
        .load_job(id)?
        .ok_or_else(|| Error::Usage(format!("no job {id}")))?;
    rmp_serde::from_slice(&bytes).map_err(|e| Error::Protocol(format!("bad job record: {e}")))
}

fn save(index: &Index, rec: &JobRecord) -> Result<()> {
    let bytes = rmp_serde::to_vec_named(rec)
        .map_err(|e| Error::Protocol(format!("cannot save job: {e}")))?;
    index.save_job(&rec.id, &bytes)
}

impl Jobs {
    pub fn new(index: Arc<Index>, workers: usize) -> Result<Self> {
        for (id, _) in index.list_jobs()? {
            let mut rec = load(&index, &id)?;
            if rec.state == JobState::Running {
                rec.crashes += 1;
                rec.updated = now();
                if rec.crashes >= MAX_STRIKES {
                    rec.state = JobState::Failed;
                    rec.error = Some(format!(
                        "gave up: the module stopped {} times while running this job (start a new scan to retry)",
                        rec.crashes
                    ));
                    index.clear_frontier(&rec.id)?;
                } else {
                    rec.state = JobState::Paused;
                    rec.error = Some(format!(
                        "the module stopped while this job was running ({} of {MAX_STRIKES} strikes)",
                        rec.crashes
                    ));
                }
                save(&index, &rec)?;
            }
        }
        Ok(Self {
            index,
            workers,
            running: Mutex::new(None),
        })
    }

    fn running(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reap(&self) {
        let mut g = self.running();
        if g.as_ref().is_some_and(|r| r.handle.is_finished())
            && let Some(r) = g.take()
        {
            let _ = r.handle.join();
        }
    }

    pub fn start_scan(&self, roots: Vec<String>) -> Result<String> {
        let mut canonical = Vec::new();
        for r in roots {
            let c = std::fs::canonicalize(&r)
                .map_err(|e| Error::Usage(format!("cannot scan {r}: {e}")))?;
            canonical.push(c.to_string_lossy().into_owned());
        }
        if canonical.is_empty() {
            return Err(Error::Usage("nothing to scan".into()));
        }
        let generation = self.index.meta(GENERATION)?.unwrap_or(0) + 1;
        let rec = JobRecord {
            id: new_job_id()?,
            roots: canonical.clone(),
            generation,
            state: JobState::Running,
            entries: 0,
            dirs: 0,
            skipped: 0,
            crashes: 0,
            error: None,
            updated: now(),
        };
        self.launch(rec, canonical, true)
    }

    pub fn resume(&self, id: &str) -> Result<String> {
        let rec = load(&self.index, id)?;
        if rec.state != JobState::Paused {
            return Err(Error::Usage(format!(
                "job {id} is {:?}, only paused jobs can be resumed",
                rec.state
            )));
        }
        let frontier = self.index.frontier(id)?;
        let mut rec = rec;
        rec.state = JobState::Running;
        rec.error = None;
        self.launch(rec, frontier, false)
    }

    fn launch(
        &self,
        mut rec: JobRecord,
        start: Vec<String>,
        new_generation: bool,
    ) -> Result<String> {
        self.reap();
        let mut slot = self.running();
        if let Some(r) = slot.as_ref() {
            return Err(Error::Usage(format!("scan {} is already running", r.id)));
        }
        if new_generation {
            self.index.set_meta(GENERATION, rec.generation)?;
        }
        rec.updated = now();
        save(&self.index, &rec)?;
        let ctl = ScanControl::new();
        let id = rec.id.clone();
        let (index, workers, thread_ctl) = (self.index.clone(), self.workers, ctl.clone());
        let handle = thread::spawn(move || {
            let result = run_scan(
                &index,
                &rec.id,
                &rec.roots,
                start,
                rec.generation,
                &thread_ctl,
                workers,
            );
            rec.entries += thread_ctl.entries.load(Ordering::SeqCst);
            rec.dirs += thread_ctl.dirs.load(Ordering::SeqCst);
            rec.skipped += thread_ctl.skipped.load(Ordering::SeqCst);
            rec.updated = now();
            match result {
                Ok(ScanEnd::Completed) => rec.state = JobState::Done,
                Ok(ScanEnd::Paused) => rec.state = JobState::Paused,
                Ok(ScanEnd::Cancelled) => rec.state = JobState::Cancelled,
                Err(e) => {
                    rec.state = JobState::Failed;
                    rec.error = Some(e.to_string());
                }
            }
            let _ = save(&index, &rec);
        });
        *slot = Some(Running {
            id: id.clone(),
            ctl,
            handle,
        });
        Ok(id)
    }

    pub fn cancel(&self, id: &str, mode: CancelMode) -> Result<()> {
        if load(&self.index, id)?.state != JobState::Running {
            return Err(Error::Usage(format!("job {id} is not running")));
        }
        let g = self.running();
        match g.as_ref() {
            Some(r) if r.id == id => {
                let flag = match mode {
                    CancelMode::Force => FORCE,
                    CancelMode::Safe => SAFE,
                    CancelMode::Unknown => {
                        return Err(Error::Usage(
                            "this module does not know that cancel mode (use safe or force)".into(),
                        ));
                    }
                };
                r.ctl.cancel.store(flag, Ordering::SeqCst);
                Ok(())
            }
            _ => Err(Error::Usage(format!("job {id} is not running"))),
        }
    }

    pub fn status(&self, id: &str) -> Result<JobStatus> {
        self.reap();
        let mut rec = load(&self.index, id)?;
        let live = self.running().as_ref().filter(|r| r.id == id).map(|r| {
            (
                r.ctl.dirs.load(Ordering::SeqCst),
                r.ctl.entries.load(Ordering::SeqCst),
                r.ctl.started.elapsed().as_secs_f64(),
                r.ctl.skipped.load(Ordering::SeqCst),
            )
        });
        if rec.state == JobState::Running && live.is_none() {
            rec.state = JobState::Failed;
            rec.error = Some("scan thread stopped unexpectedly".into());
            rec.updated = now();
            save(&self.index, &rec)?;
        }
        let skip_note = |n: u64| {
            if n > 0 {
                format!("; {n} skipped (name not valid UTF-8)")
            } else {
                String::new()
            }
        };
        let detail = match (live, &rec.error) {
            (Some((dirs, entries, secs, skipped)), _) => format!(
                "{dirs} folders walked, {entries} entries saved so far ({:.0} folders/s){}",
                dirs as f64 / secs.max(0.001),
                skip_note(skipped)
            ),
            (None, Some(e)) => format!(
                "{} entries, {} folders{}; {e}",
                rec.entries,
                rec.dirs,
                skip_note(rec.skipped)
            ),
            (None, None) => format!(
                "{} entries, {} folders{}",
                rec.entries,
                rec.dirs,
                skip_note(rec.skipped)
            ),
        };
        Ok(JobStatus {
            job: rec.id,
            state: rec.state,
            percent: None,
            eta_secs: None,
            detail: Some(detail),
        })
    }

    pub fn latest(&self) -> Result<Option<JobRecord>> {
        let mut all = Vec::new();
        for (id, _) in self.index.list_jobs()? {
            all.push(load(&self.index, &id)?);
        }
        Ok(all.into_iter().max_by_key(|r| (r.updated, r.generation)))
    }

    pub fn wait_idle(&self) {
        let r = self.running().take();
        if let Some(r) = r {
            let _ = r.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{temp_index, test_tree_root};
    use std::time::Duration;

    fn tree(tag: &str) -> std::path::PathBuf {
        let root = test_tree_root(tag);
        let _ = std::fs::remove_dir_all(&root);
        for d in ["a", "b/c"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("x.txt"), b"hello").unwrap();
        }
        root
    }

    fn wait_state(jobs: &Jobs, id: &str, want: JobState) -> JobStatus {
        let t = std::time::Instant::now();
        loop {
            let s = jobs.status(id).unwrap();
            if s.state == want {
                return s;
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "stuck in {:?}",
                s.state
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_scan_job_runs_to_done_and_reports_its_counts() {
        let (dir, ix) = temp_index("jobs-done");
        let root = tree("jobs-done");
        let jobs = Jobs::new(Arc::new(ix), 2).unwrap();
        let id = jobs
            .start_scan(vec![root.to_string_lossy().into_owned()])
            .unwrap();
        assert_eq!(id.len(), 6);
        let s = wait_state(&jobs, &id, JobState::Done);
        assert!(s.detail.unwrap().contains("entries"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_unknown_cancel_mode_is_refused_and_the_scan_keeps_going() {
        let (dir, ix) = temp_index("jobs-unknown-mode");
        let root = tree("jobs-unknown-mode");
        let jobs = Jobs::new(Arc::new(ix), 1).unwrap();
        let id = jobs
            .start_scan(vec![root.to_string_lossy().into_owned()])
            .unwrap();
        let _ = jobs.cancel(&id, CancelMode::Unknown);
        wait_state(&jobs, &id, JobState::Done);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancelling_a_job_whose_record_says_done_is_refused_even_if_the_thread_is_still_exiting() {
        let (dir, ix) = temp_index("jobs-cancel-done");
        let ix = Arc::new(ix);
        let rec = JobRecord {
            id: "d0ne01".into(),
            roots: vec![],
            generation: 1,
            state: JobState::Done,
            entries: 0,
            dirs: 0,
            skipped: 0,
            crashes: 0,
            error: None,
            updated: 1,
        };
        save(&ix, &rec).unwrap();
        let jobs = Jobs::new(ix, 1).unwrap();
        let e = jobs.cancel("d0ne01", CancelMode::Safe).unwrap_err();
        assert!(e.to_string().contains("not running"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_reports_how_many_non_utf8_names_it_skipped() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let (dir, ix) = temp_index("jobs-utf8");
        let root = tree("jobs-utf8");
        std::fs::write(root.join(OsStr::from_bytes(b"na\xefve.txt")), b"x").unwrap();
        let jobs = Jobs::new(Arc::new(ix), 2).unwrap();
        let id = jobs
            .start_scan(vec![root.to_string_lossy().into_owned()])
            .unwrap();
        let s = wait_state(&jobs, &id, JobState::Done);
        assert!(
            s.detail
                .unwrap()
                .contains("1 skipped (name not valid UTF-8)")
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_clean_job_does_not_mention_skipping() {
        let (dir, ix) = temp_index("jobs-clean");
        let root = tree("jobs-clean");
        let jobs = Jobs::new(Arc::new(ix), 2).unwrap();
        let id = jobs
            .start_scan(vec![root.to_string_lossy().into_owned()])
            .unwrap();
        let s = wait_state(&jobs, &id, JobState::Done);
        assert!(!s.detail.unwrap().contains("skipped"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn job_records_saved_before_the_skipped_box_existed_still_load() {
        #[derive(serde::Serialize)]
        struct Old {
            id: String,
            roots: Vec<String>,
            generation: u64,
            state: JobState,
            entries: u64,
            dirs: u64,
            updated: u64,
        }
        let (dir, ix) = temp_index("jobs-old-record");
        let old = Old {
            id: "01d001".into(),
            roots: vec![],
            generation: 1,
            state: JobState::Done,
            entries: 5,
            dirs: 2,
            updated: 1,
        };
        ix.save_job("01d001", &rmp_serde::to_vec_named(&old).unwrap())
            .unwrap();
        let rec = load(&ix, "01d001").unwrap();
        assert_eq!((rec.skipped, rec.error, rec.entries), (0, None, 5));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn crashed_job(ix: &Arc<Index>, crashes: u32) {
        let rec = JobRecord {
            id: "c4a5h1".into(),
            roots: vec!["/x".into()],
            generation: 1,
            state: JobState::Running,
            entries: 0,
            dirs: 0,
            skipped: 0,
            crashes,
            error: None,
            updated: 1,
        };
        save(ix, &rec).unwrap();
        ix.commit_scan("c4a5h1", &[], &["/x".to_string()]).unwrap();
    }

    #[test]
    fn a_job_that_was_running_at_three_crashes_is_given_up_on_not_resumed_forever() {
        let (dir, ix) = temp_index("jobs-strikes");
        let ix = Arc::new(ix);
        crashed_job(&ix, 0);
        for expected in [JobState::Paused, JobState::Paused, JobState::Failed] {
            let jobs = Jobs::new(ix.clone(), 1).unwrap();
            let s = jobs.status("c4a5h1").unwrap();
            assert_eq!(s.state, expected);
            if expected == JobState::Paused {
                let mut rec = load(&ix, "c4a5h1").unwrap();
                rec.state = JobState::Running;
                save(&ix, &rec).unwrap();
            } else {
                assert!(s.detail.unwrap().contains("gave up"));
                assert!(ix.frontier("c4a5h1").unwrap().is_empty());
                assert!(jobs.resume("c4a5h1").is_err());
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_strike_count_is_shown_while_a_crashed_job_waits_to_be_resumed() {
        let (dir, ix) = temp_index("jobs-strike-note");
        let ix = Arc::new(ix);
        crashed_job(&ix, 0);
        let jobs = Jobs::new(ix, 1).unwrap();
        let d = jobs.status("c4a5h1").unwrap().detail.unwrap();
        assert!(d.contains("1 of 3 strikes"), "{d}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn job_records_from_before_the_crash_counter_existed_start_at_zero() {
        #[derive(serde::Serialize)]
        struct Old {
            id: String,
            roots: Vec<String>,
            generation: u64,
            state: JobState,
            entries: u64,
            dirs: u64,
            skipped: u64,
            updated: u64,
        }
        let (dir, ix) = temp_index("jobs-old-crashes");
        let old = Old {
            id: "01d002".into(),
            roots: vec![],
            generation: 1,
            state: JobState::Paused,
            entries: 0,
            dirs: 0,
            skipped: 0,
            updated: 1,
        };
        ix.save_job("01d002", &rmp_serde::to_vec_named(&old).unwrap())
            .unwrap();
        assert_eq!(load(&ix, "01d002").unwrap().crashes, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_one_scan_runs_at_a_time() {
        let (dir, ix) = temp_index("jobs-one");
        let root = tree("jobs-one");
        let jobs = Jobs::new(Arc::new(ix), 1).unwrap();
        let r = root.to_string_lossy().into_owned();
        let first = jobs.start_scan(vec![r.clone()]).unwrap();
        let second = jobs.start_scan(vec![r]);
        if let Err(e) = second {
            assert!(e.to_string().contains("already running"), "{e}");
        }
        wait_state(&jobs, &first, JobState::Done);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_job_left_running_by_a_crash_comes_back_paused_and_resumable() {
        let (dir, ix) = temp_index("jobs-crash");
        let ix = Arc::new(ix);
        let root = tree("jobs-crash");
        let rec = JobRecord {
            id: "abc123".into(),
            roots: vec![root.to_string_lossy().into_owned()],
            generation: 1,
            state: JobState::Running,
            entries: 0,
            dirs: 0,
            skipped: 0,
            crashes: 0,
            error: None,
            updated: 1,
        };
        save(&ix, &rec).unwrap();
        ix.commit_scan("abc123", &[], std::slice::from_ref(&rec.roots[0]))
            .unwrap();
        let jobs = Jobs::new(ix.clone(), 2).unwrap();
        let s = jobs.status("abc123").unwrap();
        assert_eq!(s.state, JobState::Paused);
        jobs.resume("abc123").unwrap();
        wait_state(&jobs, "abc123", JobState::Done);
        assert!(
            ix.get(&root.join("b/c/x.txt").to_string_lossy())
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancel_and_resume_refuse_jobs_in_the_wrong_state() {
        let (dir, ix) = temp_index("jobs-wrong");
        let root = tree("jobs-wrong");
        let jobs = Jobs::new(Arc::new(ix), 2).unwrap();
        let id = jobs
            .start_scan(vec![root.to_string_lossy().into_owned()])
            .unwrap();
        wait_state(&jobs, &id, JobState::Done);
        assert!(jobs.cancel(&id, CancelMode::Safe).is_err());
        assert!(jobs.cancel(&id, CancelMode::Unknown).is_err());
        assert!(jobs.resume(&id).is_err());
        assert!(jobs.status("nope").is_err());
        assert!(
            jobs.start_scan(vec!["/definitely/not/here".into()])
                .is_err()
        );
        assert!(jobs.start_scan(vec![]).is_err());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }
}
