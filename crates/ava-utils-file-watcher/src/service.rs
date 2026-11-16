use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

use ava_utils_config::error::{Error, Result};
use ava_utils_config::messages::FilesChanged;
use ava_utils_config::topic::Publisher;

use crate::batch::{Change, Journal};
use crate::limit::{self, Limit};
use crate::roots::Roots;
use crate::watch::{SKIPPED_NON_UTF8, Watch, start};

pub const EAGER_STATE_MAX: usize = 100_000;

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
        {
            let mut j = self.journal();
            j.record(batch);
            if j.len() <= EAGER_STATE_MAX {
                self.publisher.set_state(&j.state())?;
            }
        }
        self.publisher.publish(batch)
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
                    "journal limit: {}. holding {n} changes (about {bytes} bytes)",
                    current
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
        };
        hub.emit(&b).unwrap();
        assert_eq!(hub.publisher.subscribers(), 0);
        assert_eq!(hub.held().0, 2);
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
}
