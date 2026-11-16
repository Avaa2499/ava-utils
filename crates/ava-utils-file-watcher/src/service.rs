use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

use ava_utils_config::error::{Error, Result};
use ava_utils_config::messages::FilesChanged;
use ava_utils_config::topic::Publisher;

use crate::batch::{CHUNK_BYTES, Change, Journal, split};
use crate::limit::{self, Limit};
use crate::roots::Roots;
use crate::watch::{SKIPPED_NON_UTF8, Watch, start};

pub const EAGER_STATE_MAX: usize = 100_000;
pub const UNIT_MEMORY_CAP: u64 = 8 << 30;
pub const TYPICAL_PATH_BYTES: u64 = 100;

pub fn memory_warning(limit: Limit) -> Option<String> {
    let estimate = match limit {
        Limit::Bytes(b) => b,
        Limit::Count(n) => {
            (n as u64).saturating_mul(TYPICAL_PATH_BYTES + crate::batch::ENTRY_OVERHEAD)
        }
    };
    (estimate > UNIT_MEMORY_CAP / 4 * 3).then(|| {
        "\nwarning: the service is capped at MemoryMax=8G, so this limit may get the watcher killed. Raise the cap with: systemctl --user edit ava-utils@file-watcher.service".to_string()
    })
}

pub struct Hub {
    pub publisher: Arc<Publisher>,
    journal: Mutex<Journal>,
}

impl Hub {
    pub fn new(topic: &str, limit: Limit) -> Result<Self> {
        Ok(Self {
            publisher: Publisher::new(topic, &FilesChanged::default())?,
            journal: Mutex::new(Journal::new(limit)),
        })
    }

    fn journal(&self) -> MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn emit(&self, batch: &FilesChanged) -> Result<()> {
        for part in split(batch, CHUNK_BYTES) {
            self.emit_part(&part)?;
        }
        Ok(())
    }

    fn emit_part(&self, batch: &FilesChanged) -> Result<()> {
        let event = {
            let mut j = self.journal();
            let seq = j.record(batch);
            if j.len() <= EAGER_STATE_MAX {
                self.publisher.set_state(&j.state())?;
            }
            FilesChanged {
                epoch: j.epoch(),
                seq,
                roots: j.roots().to_vec(),
                ..batch.clone()
            }
        };
        self.publisher.publish(None, &event).map(|_| ())
    }

    pub fn set_roots(&self, roots: &[String]) -> Result<()> {
        let mut j = self.journal();
        j.set_roots(roots.to_vec());
        if j.len() <= EAGER_STATE_MAX {
            self.publisher.set_state(&j.state())?;
        }
        Ok(())
    }

    pub fn refresh(&self) -> Result<()> {
        let j = self.journal();
        self.publisher.set_state(&j.state())
    }

    pub fn set_limit(&self, limit: Limit) -> Result<()> {
        let mut j = self.journal();
        j.set_limit(limit);
        self.publisher.set_state(&j.state())
    }

    pub fn held(&self) -> (usize, u64, Limit) {
        let j = self.journal();
        (j.len(), j.bytes(), j.limit())
    }
}

pub struct Service {
    state_dir: std::path::PathBuf,
    hub: Arc<Hub>,
    store: Roots,
    current: Mutex<Vec<String>>,
    watch: Mutex<Option<Watch>>,
    tx: Sender<Change>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Service {
    pub fn start(
        state_dir: &Path,
        hub: Arc<Hub>,
        tx: Sender<Change>,
    ) -> Result<(Self, Vec<String>)> {
        let store = Roots::new(state_dir);
        let roots = store.load()?;
        hub.set_roots(&roots)?;
        let (watch, warnings) = start(&roots, tx.clone())?;
        Ok((
            Self {
                state_dir: state_dir.to_path_buf(),
                hub,
                store,
                current: Mutex::new(roots),
                watch: Mutex::new(Some(watch)),
                tx,
            },
            warnings,
        ))
    }

    pub fn roots(&self) -> Vec<String> {
        lock(&self.current).clone()
    }

    pub fn set_roots(&self, roots: &[String]) -> Result<Vec<String>> {
        let roots = Roots::canonicalize_all(roots)?;
        self.store.save(&roots)?;
        let mut watch = lock(&self.watch);
        *watch = None;
        let (new, warnings) = start(&roots, self.tx.clone())?;
        *watch = Some(new);
        self.hub.set_roots(&roots)?;
        self.hub.emit(&FilesChanged::default())?;
        *lock(&self.current) = roots;
        Ok(warnings)
    }

    pub fn run_command(&self, args: &[String]) -> Result<String> {
        let rest = args.get(1..).unwrap_or(&[]);
        match args.first().map(String::as_str) {
            None => {
                let skipped = SKIPPED_NON_UTF8.load(Ordering::SeqCst);
                let note = if skipped > 0 {
                    format!(" {skipped} changes skipped (name not valid UTF-8).")
                } else {
                    String::new()
                };
                Ok(format!(
                    "file-watcher is up. watching {} folders.{note} commands: add <folder>..., remove <folder>..., list, journal [limit]",
                    self.roots().len()
                ))
            }
            Some("list") => {
                let r = self.roots();
                Ok(if r.is_empty() {
                    "not watching anything".into()
                } else {
                    r.join("\n")
                })
            }
            Some("add") => {
                if rest.is_empty() {
                    return Err(Error::Usage("usage: add <folder>...".into()));
                }
                let mut all = self.roots();
                all.extend(rest.iter().cloned());
                let warnings = self.set_roots(&all)?;
                Ok(report("watching", &self.roots(), &warnings))
            }
            Some("remove") => {
                if rest.is_empty() {
                    return Err(Error::Usage("usage: remove <folder>...".into()));
                }
                let drop: Vec<String> = rest
                    .iter()
                    .flat_map(|r| {
                        let canon = std::fs::canonicalize(r)
                            .ok()
                            .map(|c| c.to_string_lossy().into_owned());
                        [Some(r.clone()), canon].into_iter().flatten()
                    })
                    .collect();
                let keep: Vec<String> = self
                    .roots()
                    .into_iter()
                    .filter(|r| !drop.contains(r))
                    .collect();
                let warnings = self.set_roots(&keep)?;
                Ok(report("watching", &self.roots(), &warnings))
            }
            Some("journal") => {
                if let Some(text) = rest.first() {
                    let limit = Limit::parse(text)?;
                    limit::save(&self.state_dir, limit)?;
                    self.hub.set_limit(limit)?;
                }
                let (n, bytes, current) = self.hub.held();
                Ok(format!(
                    "journal limit: {current}. holding {n} changes (about {bytes} bytes){}",
                    memory_warning(current).unwrap_or_default()
                ))
            }
            Some(other) => Err(Error::Usage(format!("unknown command: {other}"))),
        }
    }
}

fn report(verb: &str, roots: &[String], warnings: &[String]) -> String {
    let mut out = vec![format!("{verb} {} folders", roots.len())];
    out.extend(warnings.iter().cloned());
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::path::PathBuf::from("/var/tmp/au-fw-tests")
            .join(format!("{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn test_hub() -> Arc<Hub> {
        Arc::new(Hub::new("files-changed", Limit::Count(100)).unwrap())
    }

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn roots_are_added_listed_removed_and_remembered() {
        let state = dir("svc-state");
        let (w1, w2) = (dir("svc-w1"), dir("svc-w2"));
        let (tx, _rx) = channel();
        let (s, _) = Service::start(&state, test_hub(), tx.clone()).unwrap();
        assert!(s.run_command(&[]).unwrap().contains("watching 0"));
        assert_eq!(
            s.run_command(&a(&["list"])).unwrap(),
            "not watching anything"
        );
        s.run_command(&a(&["add", &w1.to_string_lossy(), &w2.to_string_lossy()]))
            .unwrap();
        assert_eq!(s.roots().len(), 2);
        drop(s);
        let (s, _) = Service::start(&state, test_hub(), tx).unwrap();
        assert_eq!(s.roots().len(), 2);
        s.run_command(&a(&["remove", &w1.to_string_lossy()]))
            .unwrap();
        assert_eq!(
            s.roots(),
            [std::fs::canonicalize(&w2)
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        for p in [state, w1, w2] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn bad_commands_and_missing_folders_are_usage_errors() {
        let state = dir("svc-bad");
        let (tx, _rx) = channel();
        let (s, _) = Service::start(&state, test_hub(), tx).unwrap();
        for args in [
            &["add"][..],
            &["remove"],
            &["wat"],
            &["add", "/definitely/not/here"],
        ] {
            assert!(
                matches!(s.run_command(&a(args)), Err(Error::Usage(_))),
                "{args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn a_folder_added_later_is_watched_without_a_restart() {
        let state = dir("svc-live-state");
        let w = dir("svc-live");
        let (tx, rx) = channel();
        let (s, _) = Service::start(&state, test_hub(), tx).unwrap();
        s.run_command(&a(&["add", &w.to_string_lossy()])).unwrap();
        let f = std::fs::canonicalize(&w).unwrap().join("n.txt");
        std::fs::write(&f, b"1").unwrap();
        let want = Change::Changed(f.to_string_lossy().into_owned());
        let t = Instant::now();
        while rx.recv_timeout(Duration::from_millis(100)).ok().as_ref() != Some(&want) {
            assert!(t.elapsed() < Duration::from_secs(5));
        }
        for p in [state, w] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn the_hub_publishes_batches_and_keeps_state_for_new_subscribers() {
        let hub = Hub::new("files-changed", Limit::Count(100)).unwrap();
        let b = FilesChanged {
            changed: vec!["/a".into()],
            removed: vec!["/b".into()],
            ..Default::default()
        };
        hub.emit(&b).unwrap();
        assert_eq!(hub.publisher.subscribers(), 0);
        assert_eq!(hub.held().0, 2);
    }

    fn subscribe_to(
        hub: &Hub,
        tag: &str,
    ) -> (ava_utils_config::topic::Subscription, std::path::PathBuf) {
        let d = dir(tag);
        let sock = d.join("w.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let publisher = hub.publisher.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut c = conn.unwrap();
                let req = ava_utils_config::frame::read_frame(&mut c).unwrap();
                publisher.attach(&req, c).unwrap();
            }
        });
        (
            ava_utils_config::topic::subscribe(&sock, "files-changed").unwrap(),
            d,
        )
    }

    fn one(p: &str) -> FilesChanged {
        FilesChanged {
            changed: vec![p.into()],
            ..Default::default()
        }
    }

    #[test]
    fn events_and_state_carry_the_epoch_and_a_counting_sequence() {
        let hub = test_hub();
        hub.set_roots(&a(&["/r"])).unwrap();
        hub.emit(&one("/x")).unwrap();
        hub.emit(&one("/y")).unwrap();
        let (mut sub, d) = subscribe_to(&hub, "hub-sock-a");
        let state: FilesChanged = sub.state.body_as().unwrap();
        assert_ne!(state.epoch, 0);
        assert_eq!((state.seq, state.first_seq), (2, 1));
        assert_eq!(state.roots, ["/r"]);
        hub.emit(&one("/z")).unwrap();
        let ev: FilesChanged = sub
            .next_event(Some(std::time::Duration::from_secs(2)))
            .unwrap()
            .body_as()
            .unwrap();
        assert_eq!((ev.epoch, ev.seq), (state.epoch, 3));
        assert_eq!(ev.changed, ["/z"]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn two_hubs_have_different_epochs() {
        let (h1, h2) = (test_hub(), test_hub());
        h1.emit(&one("/x")).unwrap();
        h2.emit(&one("/x")).unwrap();
        let (mut s1, d1) = subscribe_to(&h1, "hub-sock-b1");
        let (mut s2, d2) = subscribe_to(&h2, "hub-sock-b2");
        let e1 = s1.state.body_as::<FilesChanged>().unwrap().epoch;
        let e2 = s2.state.body_as::<FilesChanged>().unwrap().epoch;
        assert_ne!(e1, e2);
        let _ = (
            &mut s1,
            &mut s2,
            std::fs::remove_dir_all(d1),
            std::fs::remove_dir_all(d2),
        );
    }

    #[test]
    fn changing_the_roots_updates_the_state_new_subscribers_see() {
        let hub = test_hub();
        hub.set_roots(&a(&["/one"])).unwrap();
        hub.set_roots(&a(&["/one", "/two"])).unwrap();
        let (sub, d) = subscribe_to(&hub, "hub-sock-c");
        assert_eq!(
            sub.state.body_as::<FilesChanged>().unwrap().roots,
            ["/one", "/two"]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn limits_that_cannot_fit_under_the_memory_cap_get_a_warning() {
        assert!(memory_warning(Limit::Count(50_000)).is_none());
        assert!(memory_warning(Limit::Bytes(50 << 20)).is_none());
        assert!(memory_warning(Limit::Bytes(6 << 30)).is_none());
        assert!(
            memory_warning(Limit::Bytes(7 << 30))
                .unwrap()
                .contains("MemoryMax=8G")
        );
        assert!(memory_warning(Limit::Bytes(18 << 30)).is_some());
        assert!(memory_warning(Limit::Count(100_000_000)).is_some());
        assert!(memory_warning(Limit::Count(u64::MAX as usize)).is_some());
    }

    #[test]
    fn the_journal_command_shows_sets_and_remembers_the_limit() {
        let state = dir("svc-journal");
        let (tx, _rx) = channel();
        let hub = test_hub();
        let (s, _) = Service::start(&state, hub.clone(), tx.clone()).unwrap();
        hub.emit(&FilesChanged {
            changed: (0..10).map(|i| format!("/{i}")).collect(),
            removed: vec![],
            ..Default::default()
        })
        .unwrap();
        assert!(
            s.run_command(&a(&["journal"]))
                .unwrap()
                .contains("100 changes")
        );
        assert!(
            s.run_command(&a(&["journal", "4"]))
                .unwrap()
                .contains("holding 4 changes")
        );
        assert!(
            s.run_command(&a(&["journal", "50mb"]))
                .unwrap()
                .contains("52428800 bytes")
        );
        assert!(matches!(
            s.run_command(&a(&["journal", "lots"])),
            Err(Error::Usage(_))
        ));
        assert_eq!(limit::load(&state).unwrap(), Limit::Bytes(50 << 20));
        let _ = std::fs::remove_dir_all(state);
    }

    #[test]
    fn adding_a_folder_tells_subscribers_with_an_event_that_lists_the_new_roots() {
        let state = dir("svc-announce-state");
        let w = dir("svc-announce");
        let (tx, _rx) = channel();
        let hub = test_hub();
        let (s, _) = Service::start(&state, hub.clone(), tx).unwrap();
        let (mut sub, d) = subscribe_to(&hub, "svc-announce-sub");
        let first = sub.state.body_as::<FilesChanged>().unwrap();
        assert!(first.roots.is_empty());
        s.run_command(&a(&["add", &w.to_string_lossy()])).unwrap();
        let ev: FilesChanged = sub
            .next_event(Some(Duration::from_secs(2)))
            .unwrap()
            .body_as()
            .unwrap();
        let root = std::fs::canonicalize(&w)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(ev.roots, [root]);
        assert!(ev.changed.is_empty() && ev.removed.is_empty());
        assert_eq!(ev.seq, first.seq + 1);
        for p in [state, w, d] {
            let _ = std::fs::remove_dir_all(p);
        }
    }

    #[test]
    fn every_event_carries_the_current_roots() {
        let hub = test_hub();
        hub.set_roots(&a(&["/r1", "/r2"])).unwrap();
        let (mut sub, d) = subscribe_to(&hub, "svc-evroots");
        hub.emit(&one("/x")).unwrap();
        let ev: FilesChanged = sub
            .next_event(Some(Duration::from_secs(2)))
            .unwrap()
            .body_as()
            .unwrap();
        assert_eq!(ev.roots, ["/r1", "/r2"]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_batch_over_the_frame_limit_reaches_a_live_subscriber_in_several_events_with_every_path() {
        let hub = Arc::new(Hub::new("files-changed", Limit::Count(1_000_000)).unwrap());
        let (mut sub, d) = subscribe_to(&hub, "svc-bigbatch");
        let pad = "p".repeat(90);
        let big = FilesChanged {
            changed: (0..250_000).map(|i| format!("/{pad}/{i:08}")).collect(),
            removed: (0..1_000).map(|i| format!("/{pad}/gone{i}")).collect(),
            ..Default::default()
        };
        let sender = {
            let hub = hub.clone();
            std::thread::spawn(move || hub.emit(&big).unwrap())
        };
        let (mut changed, mut removed, mut events, mut last_seq) = (0, 0, 0, 0);
        while changed < 250_000 || removed < 1_000 {
            let ev: FilesChanged = sub
                .next_event(Some(Duration::from_secs(60)))
                .expect("a piece went missing")
                .body_as()
                .unwrap();
            assert_eq!(
                ev.seq,
                last_seq + 1,
                "every piece has the next sequence number"
            );
            last_seq = ev.seq;
            changed += ev.changed.len();
            removed += ev.removed.len();
            events += 1;
        }
        sender.join().unwrap();
        assert!(events >= 2, "it must have been split, got {events} event");
        assert_eq!((changed, removed), (250_000, 1_000));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_state_too_big_for_one_frame_still_reaches_a_new_subscriber_and_tells_it_to_rescan() {
        let hub = Arc::new(Hub::new("files-changed", Limit::Count(1_000_000)).unwrap());
        let pad = "p".repeat(90);
        hub.emit(&FilesChanged {
            changed: (0..250_000).map(|i| format!("/{pad}/{i:08}")).collect(),
            ..Default::default()
        })
        .unwrap();
        hub.refresh().unwrap();
        let (sub, d) = subscribe_to(&hub, "svc-bigstate");
        let state = sub.state.body_as::<FilesChanged>().unwrap();
        assert_eq!(state.first_seq, state.seq + 1);
        assert!(state.changed.len() < 250_000);
        assert_ne!(state.epoch, 0);
        let _ = std::fs::remove_dir_all(d);
    }
}
