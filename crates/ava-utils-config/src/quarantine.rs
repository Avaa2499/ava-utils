use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Error, Result, codes};

pub const MAX_STRIKES: u32 = 3;

type Table = BTreeMap<String, (u32, String)>;

#[derive(Default)]
struct State {
    inflight: Table,
    strikes: Table,
}

pub struct Guard {
    dir: PathBuf,
    state: Mutex<State>,
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
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, render(t))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Guard {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn open(dir: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        let mut state = State {
            inflight: read(&dir.join("inflight.txt"))?,
            strikes: read(&dir.join("strikes.txt"))?,
        };
        for (key, (_, label)) in std::mem::take(&mut state.inflight) {
            let e = state.strikes.entry(key).or_insert((0, label));
            e.0 += 1;
        }
        write(&dir.join("strikes.txt"), &state.strikes)?;
        write(&dir.join("inflight.txt"), &state.inflight)?;
        Ok(Arc::new(Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(state),
        }))
    }

    pub fn begin(self: &Arc<Self>, key: &str, label: &str) -> Result<Ticket> {
        let mut st = self.lock();
        if let Some((n, label)) = st.strikes.get(key)
            && *n >= MAX_STRIKES
        {
            return Err(Error::Remote {
                code: codes::REFUSED.into(),
                message: format!(
                    "`{label}` crashed this module {n} times in a row (run the module's `quarantine clear` command to allow it again)"
                ),
            });
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
        write(&self.file("strikes.txt"), &st.strikes)?;
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
            st.strikes.entry(key.to_string()).or_insert((0, label)).0 += 1;
            let _ = write(&self.file("strikes.txt"), &st.strikes);
        } else if st.strikes.remove(key).is_some() {
            let _ = write(&self.file("strikes.txt"), &st.strikes);
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
}
