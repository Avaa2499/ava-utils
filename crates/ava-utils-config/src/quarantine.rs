use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result, codes};

pub const MAX_STRIKES: u32 = 3;
pub const REFUSAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

type Table = BTreeMap<String, (u32, String)>;

#[derive(Default)]
struct State {
    inflight: Table,
    strikes: Table,
    since: BTreeMap<String, u64>,
}

pub struct Guard {
    dir: PathBuf,
    state: Mutex<State>,
    ttl: Duration,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub label: String,
    pub strikes: u32,
    pub refused_for: Option<u64>,
}

fn wall_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn human(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

pub struct Ticket {
    guard: Arc<Guard>,
    key: String,
}

fn clean(label: &str) -> String {
    label
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(160)
        .collect()
}

fn parse(text: &str) -> Table {
    let mut t = Table::new();
    for line in text.lines() {
        let mut p = line.splitn(3, '\t');
        if let (Some(k), Some(n), Some(label)) = (p.next(), p.next(), p.next())
            && let Ok(n) = n.parse::<u32>()
            && !k.is_empty()
        {
            t.insert(k.to_string(), (n, label.to_string()));
        }
    }
    t
}

fn render(t: &Table) -> String {
    t.iter()
        .map(|(k, (n, label))| format!("{k}\t{n}\t{label}\n"))
        .collect()
}

fn read(path: &Path) -> Result<Table> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Ok(Table::new());
    }
    let has_trailing_newline = bytes.ends_with(b"\n");
    let text = String::from_utf8_lossy(&bytes);
    let mut table = parse(&text);
    if !has_trailing_newline {
        drop_last_line(&mut table, &text);
    }
    Ok(table)
}

fn drop_last_line(table: &mut Table, text: &str) {
    let last_line_start = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let last_line = &text[last_line_start..];
    let mut parts = last_line.splitn(3, '\t');
    if let (Some(k), Some(n), Some(_)) = (parts.next(), parts.next(), parts.next())
        && !k.is_empty()
        && n.parse::<u32>().is_ok()
    {
        table.remove(k);
    }
}

fn write(path: &Path, t: &Table) -> Result<()> {
    write_text(path, &render(t))
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_since(path: &Path) -> BTreeMap<String, u64> {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    let complete = bytes.ends_with(b"\n");
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if !complete {
        lines.pop();
    }
    lines
        .into_iter()
        .filter_map(|l| {
            let (k, secs) = l.split_once('\t')?;
            (!k.is_empty()).then_some((k.to_string(), secs.parse().ok()?))
        })
        .collect()
}

fn render_since(m: &BTreeMap<String, u64>) -> String {
    m.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect()
}

impl Guard {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn open(dir: &Path) -> Result<Arc<Self>> {
        Self::open_with(dir, REFUSAL_TTL, Box::new(wall_clock))
    }

    pub fn open_with(
        dir: &Path,
        ttl: Duration,
        clock: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let mut state = State {
            inflight: read(&dir.join("inflight.txt"))?,
            strikes: read(&dir.join("strikes.txt"))?,
            since: read_since(&dir.join("since.txt")),
        };
        for (key, (_, label)) in std::mem::take(&mut state.inflight) {
            let e = state.strikes.entry(key.clone()).or_insert((0, label));
            e.0 += 1;
            if e.0 >= MAX_STRIKES {
                state.since.entry(key).or_insert_with(&clock);
            }
        }
        let strikes = &state.strikes;
        state
            .since
            .retain(|k, _| strikes.get(k).is_some_and(|(n, _)| *n >= MAX_STRIKES));
        write(&dir.join("strikes.txt"), &state.strikes)?;
        write(&dir.join("inflight.txt"), &state.inflight)?;
        write_text(&dir.join("since.txt"), &render_since(&state.since))?;
        Ok(Arc::new(Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(state),
            ttl,
            clock,
        }))
    }

    fn save_strikes(&self, st: &State) -> Result<()> {
        write(&self.file("strikes.txt"), &st.strikes)?;
        write_text(&self.file("since.txt"), &render_since(&st.since))
    }

    pub fn begin(self: &Arc<Self>, key: &str, label: &str) -> Result<Ticket> {
        let mut st = self.lock();
        if let Some((n, label)) = st.strikes.get(key).cloned()
            && n >= MAX_STRIKES
        {
            let now = (self.clock)();
            let since = *st.since.entry(key.to_string()).or_insert(now);
            let age = now.saturating_sub(since);
            if age >= self.ttl.as_secs() {
                st.strikes.remove(key);
                st.since.remove(key);
                self.save_strikes(&st)?;
            } else {
                let left = self.ttl.as_secs() - age;
                return Err(Error::Remote {
                    code: codes::REFUSED.into(),
                    message: format!(
                        "`{label}` crashed this module {n} times in a row; refused for another {} (run the module's `quarantine clear` command to allow it sooner)",
                        human(left)
                    ),
                });
            }
        }
        let e = st
            .inflight
            .entry(key.to_string())
            .or_insert((0, clean(label)));
        e.0 += 1;
        write(&self.file("inflight.txt"), &st.inflight)?;
        Ok(Ticket {
            guard: self.clone(),
            key: key.to_string(),
        })
    }

    pub fn strikes(&self) -> Vec<(String, u32)> {
        self.lock()
            .strikes
            .values()
            .map(|(n, label)| (label.clone(), *n))
            .collect()
    }

    pub fn status(&self) -> Vec<Entry> {
        let st = self.lock();
        let now = (self.clock)();
        st.strikes
            .iter()
            .map(|(key, (n, label))| Entry {
                label: label.clone(),
                strikes: *n,
                refused_for: (*n >= MAX_STRIKES).then(|| {
                    let since = st.since.get(key).copied().unwrap_or(now);
                    self.ttl.as_secs().saturating_sub(now.saturating_sub(since))
                }),
            })
            .filter(|e| e.refused_for != Some(0))
            .collect()
    }

    pub fn refused(&self) -> Vec<(String, u32)> {
        self.strikes()
            .into_iter()
            .filter(|(_, n)| *n >= MAX_STRIKES)
            .collect()
    }

    pub fn clear(&self, needle: &str) -> Result<usize> {
        let mut st = self.lock();
        let before = st.strikes.len();
        if needle.is_empty() || needle == "all" {
            st.strikes.clear();
        } else {
            st.strikes.retain(|_, (_, label)| !label.contains(needle));
        }
        let strikes = &st.strikes.clone();
        st.since.retain(|k, _| strikes.contains_key(k));
        self.save_strikes(&st)?;
        Ok(before - st.strikes.len())
    }

    fn finish(&self, key: &str, panicked: bool) {
        let mut st = self.lock();
        let gone = match st.inflight.get_mut(key) {
            Some(e) if e.0 > 1 => {
                e.0 -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        let label = if gone {
            st.inflight.remove(key).map(|(_, l)| l)
        } else {
            None
        };
        let _ = write(&self.file("inflight.txt"), &st.inflight);
        if panicked {
            let label = label.unwrap_or_default();
            let n = {
                let e = st.strikes.entry(key.to_string()).or_insert((0, label));
                e.0 += 1;
                e.0
            };
            if n >= MAX_STRIKES {
                let now = (self.clock)();
                st.since.entry(key.to_string()).or_insert(now);
            }
            let _ = self.save_strikes(&st);
        } else if st.strikes.remove(key).is_some() {
            st.since.remove(key);
            let _ = self.save_strikes(&st);
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.guard.finish(&self.key, std::thread::panicking());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn crash(dir: &Path, key: &str, label: &str) {
        let g = Guard::open(dir).unwrap();
        let t = g.begin(key, label).unwrap();
        std::mem::forget(t);
    }

    #[test]
    fn finished_work_leaves_no_trace() {
        let d = TempDir::new();
        let g = Guard::open(d.path()).unwrap();
        drop(g.begin("k1", "scan /home/a").unwrap());
        assert!(g.strikes().is_empty());
        assert_eq!(
            std::fs::read_to_string(d.path().join("inflight.txt")).unwrap(),
            ""
        );
        assert!(Guard::open(d.path()).unwrap().strikes().is_empty());
    }

    #[test]
    fn three_crashes_on_the_same_thing_refuse_it_and_only_it() {
        let d = TempDir::new();
        for _ in 0..3 {
            crash(d.path(), "poison", "scan /bad");
        }
        let g = Guard::open(d.path()).unwrap();
        match g.begin("poison", "scan /bad") {
            Err(Error::Remote { code, message }) => {
                assert_eq!(code, codes::REFUSED);
                assert!(
                    message.contains("scan /bad") && message.contains("quarantine clear"),
                    "{message}"
                );
            }
            other => panic!("expected a refusal, got {:?}", other.is_ok()),
        }
        assert!(g.begin("fine", "scan /good").is_ok());
        assert_eq!(g.refused().len(), 1);
    }

    #[test]
    fn two_crashes_are_not_enough() {
        let d = TempDir::new();
        crash(d.path(), "k", "work");
        crash(d.path(), "k", "work");
        let g = Guard::open(d.path()).unwrap();
        assert!(g.refused().is_empty());
        assert_eq!(g.strikes(), [("work".to_string(), 2)]);
        assert!(g.begin("k", "work").is_ok());
    }

    #[test]
    fn a_normal_finish_resets_the_strikes_so_it_means_three_in_a_row() {
        let d = TempDir::new();
        crash(d.path(), "k", "work");
        crash(d.path(), "k", "work");
        let g = Guard::open(d.path()).unwrap();
        drop(g.begin("k", "work").unwrap());
        assert!(g.strikes().is_empty());
        crash(d.path(), "k", "work");
        crash(d.path(), "k", "work");
        assert!(Guard::open(d.path()).unwrap().begin("k", "work").is_ok());
    }

    #[test]
    fn the_same_thing_twice_at_once_stays_in_flight_until_both_finish() {
        let d = TempDir::new();
        let g = Guard::open(d.path()).unwrap();
        let (a, b) = (g.begin("k", "work").unwrap(), g.begin("k", "work").unwrap());
        drop(a);
        assert!(
            std::fs::read_to_string(d.path().join("inflight.txt"))
                .unwrap()
                .contains("work")
        );
        drop(b);
        assert_eq!(
            std::fs::read_to_string(d.path().join("inflight.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_panic_in_the_work_counts_as_a_strike() {
        let d = TempDir::new();
        let g = Guard::open(d.path()).unwrap();
        for _ in 0..3 {
            let g2 = g.clone();
            let r = std::thread::spawn(move || {
                let _t = g2.begin("k", "bad input").unwrap();
                panic!("handler bug");
            })
            .join();
            assert!(r.is_err());
        }
        assert!(g.begin("k", "bad input").is_err());
    }

    #[test]
    fn clear_lifts_a_refusal_by_name_or_everything() {
        let d = TempDir::new();
        for _ in 0..3 {
            crash(d.path(), "a", "scan /one");
            crash(d.path(), "b", "scan /two");
        }
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.refused().len(), 2);
        assert_eq!(g.clear("/one").unwrap(), 1);
        assert!(g.begin("a", "scan /one").is_ok());
        assert!(g.begin("b", "scan /two").is_err());
        assert_eq!(g.clear("all").unwrap(), 1);
        assert!(g.begin("b", "scan /two").is_ok());
        assert_eq!(g.clear("nothing-matches").unwrap(), 0);
        assert!(Guard::open(d.path()).unwrap().refused().is_empty());
    }

    #[test]
    fn labels_with_tabs_and_newlines_cannot_corrupt_the_files() {
        let d = TempDir::new();
        let nasty = "scan\t/a\nb\r\n9\tfake";
        for _ in 0..3 {
            crash(d.path(), "k", nasty);
        }
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.refused().len(), 1);
        assert_eq!(g.strikes().len(), 1);
    }

    #[test]
    fn garbage_in_the_files_is_ignored_not_fatal() {
        let d = TempDir::new();
        std::fs::write(
            d.path().join("strikes.txt"),
            "no tabs here\n\tonly\n\n k\tnotanumber\tx\nok\t2\twork\n",
        )
        .unwrap();
        std::fs::write(d.path().join("inflight.txt"), "\u{0}\u{1}\n").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 2)]);
    }

    // --- Corruption robustness tests ---

    #[test]
    fn empty_files_are_ok() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert!(g.strikes().is_empty());
        assert!(g.begin("new", "fresh").is_ok());
        drop(g.begin("new", "fresh").unwrap());
        assert!(Guard::open(d.path()).unwrap().strikes().is_empty());
    }

    #[test]
    fn strikes_truncated_mid_line_is_ignored() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "good\t1\twork\ntrunc").unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn inflight_truncated_mid_line_keeps_good_line() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        std::fs::write(d.path().join("inflight.txt"), "key\t1\tlabel\ntrunc").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("label".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn strikes_binary_bytes_are_ignored() {
        let d = TempDir::new();
        std::fs::write(
            d.path().join("strikes.txt"),
            b"good\t1\twork\n\xff\xfe\x00\x01bad",
        )
        .unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn inflight_binary_bytes_keeps_good_line() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        std::fs::write(
            d.path().join("inflight.txt"),
            b"key\t1\tlabel\n\xff\xfe\x00\x01",
        )
        .unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("label".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn strikes_invalid_utf8_is_handled() {
        let d = TempDir::new();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"good\t1\twork\n");
        bytes.extend_from_slice(&[0x80, 0x81, 0x82]); // invalid UTF-8
        std::fs::write(d.path().join("strikes.txt"), bytes).unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn inflight_invalid_utf8_keeps_good_line() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"key\t1\tlabel\n");
        bytes.extend_from_slice(&[0x80, 0x81, 0x82]);
        std::fs::write(d.path().join("inflight.txt"), bytes).unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("label".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn strikes_good_line_then_garbage_keeps_good() {
        let d = TempDir::new();
        std::fs::write(
            d.path().join("strikes.txt"),
            "good\t1\twork\ngarbage\nmore\tnotanumber\tbad\n",
        )
        .unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn inflight_good_line_then_garbage_keeps_good() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        std::fs::write(
            d.path().join("inflight.txt"),
            "key\t1\tlabel\ngarbage\nmore\tnotanumber\tbad\n",
        )
        .unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("label".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn strikes_garbage_then_good_line_keeps_good() {
        let d = TempDir::new();
        std::fs::write(
            d.path().join("strikes.txt"),
            "garbage\nmore\tnotanumber\tbad\ngood\t1\twork\n",
        )
        .unwrap();
        std::fs::write(d.path().join("inflight.txt"), "").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn inflight_garbage_then_good_line_keeps_good() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "").unwrap();
        std::fs::write(
            d.path().join("inflight.txt"),
            "garbage\nmore\tnotanumber\tbad\nkey\t1\tlabel\n",
        )
        .unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("label".to_string(), 1)]);
        assert!(g.begin("new", "fresh").is_ok());
    }

    #[test]
    fn after_corrupt_open_begin_finish_clear_still_work() {
        let d = TempDir::new();
        let mut strikes = Vec::new();
        strikes.extend_from_slice(b"bad\ngood\t1\twork\n");
        strikes.extend_from_slice(&[0xff, 0xfe]);
        std::fs::write(d.path().join("strikes.txt"), strikes).unwrap();
        std::fs::write(d.path().join("inflight.txt"), "trunc").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);

        let t = g.begin("new", "fresh").unwrap();
        drop(t);
        assert_eq!(g.strikes(), [("work".to_string(), 1)]); // original strike remains

        let t2 = g.begin("another", "more").unwrap();
        drop(t2);
        assert_eq!(g.strikes(), [("work".to_string(), 1)]);

        g.clear("all").unwrap();
        assert!(Guard::open(d.path()).unwrap().strikes().is_empty());
    }

    #[test]
    fn a_complete_looking_last_line_without_a_newline_was_half_written_and_is_dropped() {
        for file in ["strikes.txt", "inflight.txt"] {
            let d = TempDir::new();
            std::fs::write(d.path().join(file), "good\t1\twork\nhalf\t2\tlabel").unwrap();
            let g = Guard::open(d.path()).unwrap();
            assert_eq!(g.strikes(), [("work".to_string(), 1)], "{file}");
        }
    }

    #[test]
    fn a_last_line_that_ends_with_a_newline_is_kept() {
        let d = TempDir::new();
        std::fs::write(
            d.path().join("strikes.txt"),
            "good\t1\twork\nfull\t2\tlabel\n",
        )
        .unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert_eq!(g.strikes().len(), 2);
    }

    #[test]
    fn a_single_line_without_a_newline_that_looks_complete_is_dropped() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "only\t3\tlabel").unwrap();
        let g = Guard::open(d.path()).unwrap();
        assert!(g.strikes().is_empty());
        assert!(
            g.begin("only", "label").is_ok(),
            "a half-written strike must not refuse anything"
        );
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    const DAY: u64 = 24 * 60 * 60;

    fn clock(start: u64) -> (Arc<AtomicU64>, Box<dyn Fn() -> u64 + Send + Sync>) {
        let t = Arc::new(AtomicU64::new(start));
        let t2 = t.clone();
        (t, Box::new(move || t2.load(Ordering::SeqCst)))
    }

    fn open_at(dir: &Path, t: &Arc<AtomicU64>) -> Arc<Guard> {
        let t = t.clone();
        Guard::open_with(
            dir,
            Duration::from_secs(DAY),
            Box::new(move || t.load(Ordering::SeqCst)),
        )
        .unwrap()
    }

    fn poison(dir: &Path, t: &Arc<AtomicU64>, key: &str, label: &str) {
        for _ in 0..3 {
            let g = open_at(dir, t);
            std::mem::forget(g.begin(key, label).unwrap());
        }
        drop(open_at(dir, t));
    }

    fn refused(g: &Arc<Guard>, key: &str) -> bool {
        matches!(g.begin(key, "x"), Err(Error::Remote { ref code, .. }) if code == codes::REFUSED)
    }

    #[test]
    fn a_refusal_lasts_a_day_then_the_request_is_allowed_again_with_a_fresh_start() {
        let d = TempDir::new();
        let (t, _) = clock(1_000);
        poison(d.path(), &t, "k", "scan /bad");
        let g = open_at(d.path(), &t);
        assert!(refused(&g, "k"));
        t.store(1_000 + DAY - 1, Ordering::SeqCst);
        assert!(refused(&g, "k"), "one second early");
        t.store(1_000 + DAY, Ordering::SeqCst);
        drop(g.begin("k", "scan /bad").expect("expired"));
        assert!(g.strikes().is_empty(), "the old strikes are gone");
        assert!(!refused(
            &Guard::open_with(d.path(), Duration::from_secs(DAY), Box::new(|| 1_000 + DAY))
                .unwrap(),
            "k"
        ));
    }

    #[test]
    fn after_expiry_it_takes_three_more_crashes_to_refuse_again() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "k", "work");
        t.store(DAY + 5, Ordering::SeqCst);
        drop(open_at(d.path(), &t).begin("k", "work").unwrap());
        for _ in 0..2 {
            let g = open_at(d.path(), &t);
            std::mem::forget(g.begin("k", "work").unwrap());
        }
        assert!(
            !refused(&open_at(d.path(), &t), "k"),
            "two crashes are not enough"
        );
        poison(d.path(), &t, "k", "work");
        assert!(refused(&open_at(d.path(), &t), "k"));
    }

    #[test]
    fn the_refusal_message_says_how_long_is_left() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "k", "scan /bad");
        t.store(3600, Ordering::SeqCst);
        match open_at(d.path(), &t).begin("k", "x") {
            Err(Error::Remote { message, .. }) => {
                assert!(message.contains("refused for another 23h 0m"), "{message}");
                assert!(message.contains("quarantine clear"), "{message}");
            }
            other => panic!("expected a refusal, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn asking_again_while_refused_does_not_restart_the_clock() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "k", "work");
        let g = open_at(d.path(), &t);
        for at in [10, 1000, DAY - 1] {
            t.store(at, Ordering::SeqCst);
            assert!(refused(&g, "k"));
        }
        t.store(DAY, Ordering::SeqCst);
        assert!(g.begin("k", "work").is_ok());
    }

    #[test]
    fn the_refusal_time_is_remembered_across_restarts() {
        let d = TempDir::new();
        let (t, _) = clock(500);
        poison(d.path(), &t, "k", "work");
        t.store(500 + DAY / 2, Ordering::SeqCst);
        let again = open_at(d.path(), &t);
        assert!(refused(&again, "k"));
        t.store(500 + DAY, Ordering::SeqCst);
        assert!(open_at(d.path(), &t).begin("k", "work").is_ok());
    }

    #[test]
    fn old_state_without_a_time_file_starts_the_clock_when_first_seen() {
        let d = TempDir::new();
        std::fs::write(d.path().join("strikes.txt"), "k\t3\twork\n").unwrap();
        let (t, _) = clock(7_000);
        let g = open_at(d.path(), &t);
        assert!(
            refused(&g, "k"),
            "not expired just because no time was recorded"
        );
        t.store(7_000 + DAY, Ordering::SeqCst);
        assert!(g.begin("k", "work").is_ok());
    }

    #[test]
    fn a_damaged_time_file_is_ignored_and_never_blocks_opening() {
        for junk in [
            &b"\xff\xfe\x00"[..],
            b"k\tnotanumber\n",
            b"k\t12",
            b"\n\n\t\t\n",
            b"",
        ] {
            let d = TempDir::new();
            std::fs::write(d.path().join("strikes.txt"), "k\t3\twork\n").unwrap();
            std::fs::write(d.path().join("since.txt"), junk).unwrap();
            let (t, _) = clock(100);
            let g = open_at(d.path(), &t);
            assert!(refused(&g, "k"), "{junk:?}");
            t.store(100 + DAY, Ordering::SeqCst);
            assert!(g.begin("k", "work").is_ok(), "{junk:?}");
        }
    }

    #[test]
    fn clearing_removes_the_time_too_so_a_later_refusal_starts_a_new_clock() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "k", "work");
        let g = open_at(d.path(), &t);
        assert_eq!(g.clear("work").unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(d.path().join("since.txt")).unwrap(),
            ""
        );
        t.store(DAY / 2, Ordering::SeqCst);
        poison(d.path(), &t, "k", "work");
        t.store(DAY + 10, Ordering::SeqCst);
        assert!(
            refused(&open_at(d.path(), &t), "k"),
            "the new clock started at DAY/2"
        );
    }

    #[test]
    fn a_normal_finish_removes_any_time_record() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "k", "work");
        t.store(DAY, Ordering::SeqCst);
        let g = open_at(d.path(), &t);
        drop(g.begin("k", "work").unwrap());
        assert_eq!(
            std::fs::read_to_string(d.path().join("since.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn expiry_of_one_request_leaves_other_refusals_alone() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "old", "scan /old");
        t.store(DAY / 2, Ordering::SeqCst);
        poison(d.path(), &t, "new", "scan /new");
        t.store(DAY + 1, Ordering::SeqCst);
        let g = open_at(d.path(), &t);
        assert!(g.begin("old", "x").is_ok());
        assert!(refused(&g, "new"));
    }

    #[test]
    fn a_handler_panic_that_makes_the_third_strike_starts_the_clock() {
        let d = TempDir::new();
        let (t, _) = clock(50);
        let g = open_at(d.path(), &t);
        for _ in 0..3 {
            let g2 = g.clone();
            let _ = std::thread::spawn(move || {
                let _ticket = g2.begin("k", "work").unwrap();
                panic!("handler bug");
            })
            .join();
        }
        assert!(refused(&g, "k"));
        t.store(50 + DAY, Ordering::SeqCst);
        assert!(g.begin("k", "work").is_ok());
    }

    #[test]
    fn status_lists_watching_and_refused_entries_with_time_left_and_hides_expired_ones() {
        let d = TempDir::new();
        let (t, _) = clock(0);
        poison(d.path(), &t, "bad", "scan /bad");
        for _ in 0..2 {
            let g = open_at(d.path(), &t);
            std::mem::forget(g.begin("meh", "scan /meh").unwrap());
        }
        t.store(DAY - 60, Ordering::SeqCst);
        let g = open_at(d.path(), &t);
        let mut s = g.status();
        s.sort_by(|a, b| a.label.cmp(&b.label));
        assert_eq!(
            s,
            [
                Entry {
                    label: "scan /bad".into(),
                    strikes: 3,
                    refused_for: Some(60)
                },
                Entry {
                    label: "scan /meh".into(),
                    strikes: 2,
                    refused_for: None
                },
            ]
        );
        t.store(DAY, Ordering::SeqCst);
        assert_eq!(
            g.status().len(),
            1,
            "the expired refusal is no longer listed"
        );
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human(0), "0s");
        assert_eq!(human(59), "59s");
        assert_eq!(human(60), "1m 0s");
        assert_eq!(human(3599), "59m 59s");
        assert_eq!(human(3600), "1h 0m");
        assert_eq!(human(DAY), "24h 0m");
    }
}
