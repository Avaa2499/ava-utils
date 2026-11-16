use std::path::Path;
use std::sync::mpsc::Sender;

use ava_utils_config::error::{Error, Result};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::batch::Change;

pub const NEW_FOLDER_LIST_CAP: usize = 100_000;

pub struct Watch {
    _watcher: RecommendedWatcher,
}

pub fn list_tree(dir: &Path, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            if out.len() >= cap {
                return out;
            }
            let p = e.path();
            if let Some(s) = p.to_str() {
                out.push(s.to_string());
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                todo.push(p);
            }
        }
    }
    out
}

fn exists_now(p: &Path, tx: &Sender<Change>, walk_new_dirs: bool) {
    let Some(s) = p.to_str() else { return };
    match std::fs::symlink_metadata(p) {
        Ok(meta) => {
            let _ = tx.send(Change::Changed(s.to_string()));
            if walk_new_dirs && meta.is_dir() {
                for child in list_tree(p, NEW_FOLDER_LIST_CAP) {
                    let _ = tx.send(Change::Changed(child));
                }
            }
        }
        Err(_) => {
            let _ = tx.send(Change::Removed(s.to_string()));
        }
    }
}

pub fn translate(event: &Event, tx: &Sender<Change>) {
    let removed = |p: &Path| {
        if let Some(s) = p.to_str() {
            let _ = tx.send(Change::Removed(s.to_string()));
        }
    };
    match &event.kind {
        EventKind::Access(_) => {}
        EventKind::Remove(_) => event.paths.iter().for_each(|p| removed(p)),
        EventKind::Create(_) => event.paths.iter().for_each(|p| exists_now(p, tx, true)),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            event.paths.iter().for_each(|p| removed(p))
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            event.paths.iter().for_each(|p| exists_now(p, tx, true))
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let [from, to] = event.paths.as_slice() {
                removed(from);
                exists_now(to, tx, true);
            }
        }
        _ => event.paths.iter().for_each(|p| exists_now(p, tx, false)),
    }
}

pub fn start(roots: &[String], tx: Sender<Change>) -> Result<(Watch, Vec<String>)> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res {
            translate(&event, &tx);
        }
    })
    .map_err(|e| Error::Protocol(format!("cannot start the file watcher: {e}")))?;
    let mut warnings = Vec::new();
    for r in roots {
        if let Err(e) = watcher.watch(Path::new(r), RecursiveMode::Recursive) {
            warnings.push(format!("cannot watch {r}: {e}"));
        }
    }
    Ok((Watch { _watcher: watcher }, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::path::PathBuf::from("/var/tmp/au-fw-tests")
            .join(format!("{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait_for(rx: &Receiver<Change>, want: &Change) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if let Ok(c) = rx.recv_timeout(Duration::from_millis(100))
                && &c == want
            {
                return;
            }
        }
        panic!("never saw {want:?}");
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn new_changed_and_removed_files_are_reported() {
        let d = dir("basic");
        let (tx, rx) = channel();
        let (_w, warnings) = start(&[s(&d)], tx).unwrap();
        assert!(warnings.is_empty());
        let f = d.join("a.txt");
        std::fs::write(&f, b"1").unwrap();
        wait_for(&rx, &Change::Changed(s(&f)));
        std::fs::write(&f, b"22").unwrap();
        wait_for(&rx, &Change::Changed(s(&f)));
        std::fs::remove_file(&f).unwrap();
        wait_for(&rx, &Change::Removed(s(&f)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn renames_report_the_old_name_removed_and_the_new_one_changed() {
        let d = dir("rename");
        let (from, to) = (d.join("old.txt"), d.join("new.txt"));
        std::fs::write(&from, b"1").unwrap();
        let (tx, rx) = channel();
        let (_w, _) = start(&[s(&d)], tx).unwrap();
        std::fs::rename(&from, &to).unwrap();
        wait_for(&rx, &Change::Removed(s(&from)));
        wait_for(&rx, &Change::Changed(s(&to)));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn files_inside_a_brand_new_folder_are_reported_and_watched() {
        let d = dir("newdir");
        let (tx, rx) = channel();
        let (_w, _) = start(&[s(&d)], tx).unwrap();
        let sub = d.join("sub/deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("x.txt"), b"1").unwrap();
        wait_for(&rx, &Change::Changed(s(&sub.join("x.txt"))));
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(sub.join("later.txt"), b"1").unwrap();
        wait_for(&rx, &Change::Changed(s(&sub.join("later.txt"))));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_missing_root_is_a_warning_and_the_others_still_work() {
        let d = dir("warn");
        let (tx, rx) = channel();
        let (_w, warnings) = start(&["/definitely/not/here".into(), s(&d)], tx).unwrap();
        assert_eq!(warnings.len(), 1);
        std::fs::write(d.join("ok.txt"), b"1").unwrap();
        wait_for(&rx, &Change::Changed(s(&d.join("ok.txt"))));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn list_tree_is_capped() {
        let d = dir("cap");
        for i in 0..20 {
            std::fs::write(d.join(format!("{i}.txt")), b"1").unwrap();
        }
        assert_eq!(list_tree(&d, 5).len(), 5);
        let all: BTreeSet<String> = list_tree(&d, 100).into_iter().collect();
        assert_eq!(all.len(), 20);
        let _ = std::fs::remove_dir_all(d);
    }
}
