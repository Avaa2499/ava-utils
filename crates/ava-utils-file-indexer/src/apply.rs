use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

use ava_utils_config::error::Result;
use ava_utils_config::messages::FilesChanged;

use crate::db::{Entry, Index, Record};
use crate::exclude::{is_excluded, is_virtual};

pub const GENERATION: &str = "generation";

pub fn entry_from_meta(path: &str, meta: &Metadata, generation: u64) -> Entry {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Entry {
        path: path.to_string(),
        record: Record {
            size: if meta.is_dir() { 0 } else { meta.len() },
            mtime,
            inode: meta.ino(),
            is_dir: meta.is_dir(),
            generation,
        },
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub upserted: u64,
    pub removed: u64,
    pub skipped: u64,
}

pub fn apply_changes(index: &Index, batch: &FilesChanged) -> Result<Applied> {
    let exclusions = index.exclusions()?;
    let generation = index.meta(GENERATION)?.unwrap_or(0);
    let mut out = Applied::default();
    let mut upserts = Vec::new();
    for path in &batch.removed {
        out.removed += index.remove_tree(path)?;
    }
    for path in &batch.changed {
        if is_virtual(Path::new(path)) {
            out.skipped += 1;
            continue;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if is_excluded(path, meta.is_dir(), &exclusions) {
                    out.skipped += 1;
                } else {
                    upserts.push(entry_from_meta(path, &meta, generation));
                }
            }
            Err(_) => out.removed += index.remove_tree(path)?,
        }
    }
    out.upserted = upserts.len() as u64;
    index.upsert(&upserts)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::temp_index;

    fn tree(tag: &str) -> std::path::PathBuf {
        let d = crate::db::test_tree_root(tag);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), b"12345").unwrap();
        d
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn changed_paths_are_stat_ed_and_indexed() {
        let (dir, ix) = temp_index("apply-changed");
        let t = tree("changed");
        let f = s(&t.join("src/a.rs"));
        let r = apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![f.clone()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.upserted, 1);
        assert_eq!(ix.get(&f).unwrap().unwrap().size, 5);
        std::fs::write(&f, b"1234567890").unwrap();
        apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![f.clone()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(ix.get(&f).unwrap().unwrap().size, 10);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn a_changed_path_that_no_longer_exists_is_removed() {
        let (dir, ix) = temp_index("apply-gone");
        let t = tree("gone");
        let f = s(&t.join("src/a.rs"));
        apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![f.clone()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::remove_file(&f).unwrap();
        let r = apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![f.clone()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.removed, 1);
        assert_eq!(ix.get(&f).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn removed_folders_take_their_contents() {
        let (dir, ix) = temp_index("apply-removed");
        let t = tree("removed");
        let (folder, f) = (s(&t.join("src")), s(&t.join("src/a.rs")));
        apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![folder.clone(), f.clone()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let r = apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![],
                removed: vec![folder],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.removed, 2);
        assert_eq!(ix.get(&f).unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn excluded_and_virtual_paths_are_skipped() {
        let (dir, ix) = temp_index("apply-skip");
        let t = tree("skip");
        std::fs::create_dir_all(t.join("target")).unwrap();
        std::fs::write(t.join("target/x.o"), b"x").unwrap();
        ix.add_exclusion("target/").unwrap();
        let r = apply_changes(
            &ix,
            &FilesChanged {
                changed: vec![s(&t.join("target/x.o")), "/proc/self/status".into()],
                removed: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((r.upserted, r.skipped), (0, 2));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(t);
    }

    #[test]
    fn an_empty_batch_is_fine() {
        let (dir, ix) = temp_index("apply-empty");
        assert_eq!(
            apply_changes(&ix, &FilesChanged::default()).unwrap(),
            Applied::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
