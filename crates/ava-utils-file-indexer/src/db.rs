use std::collections::{HashMap, HashSet};
use std::path::Path;

use ava_utils_config::error::{Error, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

pub const SCHEMA_VERSION: u64 = 1;

pub(crate) const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
pub(crate) const EXT_NAME: TableDefinition<&str, ()> = TableDefinition::new("ext_name");
pub(crate) const NAME_PATH: TableDefinition<&str, ()> = TableDefinition::new("name_path");
pub(crate) const FOLDER_SIZES: TableDefinition<&str, u64> = TableDefinition::new("folder_sizes");
pub(crate) const EXCLUSIONS: TableDefinition<&str, ()> = TableDefinition::new("exclusions");
pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
pub(crate) const JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("jobs");
pub(crate) const FRONTIER: TableDefinition<&str, ()> = TableDefinition::new("frontier");

pub fn db_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> Error {
    Error::Io(std::io::Error::other(e))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub size: u64,
    pub mtime: u64,
    pub inode: u64,
    pub is_dir: bool,
    pub generation: u64,
}

impl Record {
    pub fn to_bytes(&self) -> [u8; 33] {
        let mut b = [0u8; 33];
        b[0..8].copy_from_slice(&self.size.to_le_bytes());
        b[8..16].copy_from_slice(&self.mtime.to_le_bytes());
        b[16..24].copy_from_slice(&self.inode.to_le_bytes());
        b[24] = self.is_dir as u8;
        b[25..33].copy_from_slice(&self.generation.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 33 {
            return None;
        }
        Some(Self {
            size: u64::from_le_bytes(b[0..8].try_into().ok()?),
            mtime: u64::from_le_bytes(b[8..16].try_into().ok()?),
            inode: u64::from_le_bytes(b[16..24].try_into().ok()?),
            is_dir: b[24] != 0,
            generation: u64::from_le_bytes(b[25..33].try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub record: Record,
}

pub fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn ext_of(path: &str) -> String {
    let name = name_of(path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

pub(crate) fn name_key(path: &str) -> String {
    format!("{}\0{path}", name_of(path).to_lowercase())
}

pub(crate) fn ext_key(path: &str) -> String {
    format!("{}\0{}\0{path}", ext_of(path), name_of(path).to_lowercase())
}

fn after_prefix(dir: &str) -> (String, String) {
    (format!("{dir}/"), format!("{dir}0"))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Finalized {
    pub stale_removed: u64,
    pub folders: u64,
}

pub struct Index {
    pub(crate) db: Database,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Database::create(path).map_err(db_err)?;
        let index = Self { db };
        let w = index.db.begin_write().map_err(db_err)?;
        {
            w.open_table(FILES).map_err(db_err)?;
            w.open_table(EXT_NAME).map_err(db_err)?;
            w.open_table(NAME_PATH).map_err(db_err)?;
            w.open_table(FOLDER_SIZES).map_err(db_err)?;
            w.open_table(EXCLUSIONS).map_err(db_err)?;
            w.open_table(JOBS).map_err(db_err)?;
            w.open_table(FRONTIER).map_err(db_err)?;
            let mut meta = w.open_table(META).map_err(db_err)?;
            let have = meta.get("schema").map_err(db_err)?.map(|g| g.value());
            match have {
                None => {
                    meta.insert("schema", SCHEMA_VERSION).map_err(db_err)?;
                }
                Some(v) if v == SCHEMA_VERSION => {}
                Some(v) => {
                    return Err(Error::Usage(format!(
                        "index has schema {v}, this build understands {SCHEMA_VERSION}"
                    )));
                }
            }
        }
        w.commit().map_err(db_err)?;
        Ok(index)
    }

    pub fn upsert(&self, entries: &[Entry]) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        {
            let mut files = w.open_table(FILES).map_err(db_err)?;
            let mut names = w.open_table(NAME_PATH).map_err(db_err)?;
            let mut exts = w.open_table(EXT_NAME).map_err(db_err)?;
            for e in entries {
                files
                    .insert(e.path.as_str(), e.record.to_bytes().as_slice())
                    .map_err(db_err)?;
                names
                    .insert(name_key(&e.path).as_str(), ())
                    .map_err(db_err)?;
                if !e.record.is_dir {
                    exts.insert(ext_key(&e.path).as_str(), ()).map_err(db_err)?;
                }
            }
        }
        w.commit().map_err(db_err)
    }

    pub fn commit_scan(&self, job: &str, entries: &[Entry], frontier: &[String]) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        {
            let mut files = w.open_table(FILES).map_err(db_err)?;
            let mut names = w.open_table(NAME_PATH).map_err(db_err)?;
            let mut exts = w.open_table(EXT_NAME).map_err(db_err)?;
            for e in entries {
                files
                    .insert(e.path.as_str(), e.record.to_bytes().as_slice())
                    .map_err(db_err)?;
                names
                    .insert(name_key(&e.path).as_str(), ())
                    .map_err(db_err)?;
                if !e.record.is_dir {
                    exts.insert(ext_key(&e.path).as_str(), ()).map_err(db_err)?;
                }
            }
            let mut fr = w.open_table(FRONTIER).map_err(db_err)?;
            let (from, to) = (format!("{job}\0"), format!("{job}\u{1}"));
            let old: Vec<String> = fr
                .range(from.as_str()..to.as_str())
                .map_err(db_err)?
                .map(|r| r.map(|(k, _)| k.value().to_string()))
                .collect::<std::result::Result<_, _>>()
                .map_err(db_err)?;
            for k in old {
                fr.remove(k.as_str()).map_err(db_err)?;
            }
            for dir in frontier {
                fr.insert(format!("{job}\0{dir}").as_str(), ())
                    .map_err(db_err)?;
            }
        }
        w.commit().map_err(db_err)
    }

    pub fn frontier(&self, job: &str) -> Result<Vec<String>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let fr = r.open_table(FRONTIER).map_err(db_err)?;
        let (from, to) = (format!("{job}\0"), format!("{job}\u{1}"));
        let mut out = Vec::new();
        for row in fr.range(from.as_str()..to.as_str()).map_err(db_err)? {
            let k = row.map_err(db_err)?.0;
            if let Some((_, dir)) = k.value().split_once('\0') {
                out.push(dir.to_string());
            }
        }
        Ok(out)
    }

    pub fn clear_frontier(&self, job: &str) -> Result<()> {
        self.commit_scan(job, &[], &[])
    }

    pub fn save_job(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(JOBS)
            .map_err(db_err)?
            .insert(id, bytes)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    pub fn load_job(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(JOBS).map_err(db_err)?;
        Ok(t.get(id).map_err(db_err)?.map(|g| g.value().to_vec()))
    }

    pub fn list_jobs(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(JOBS).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(db_err)? {
            let (k, v) = row.map_err(db_err)?;
            out.push((k.value().to_string(), v.value().to_vec()));
        }
        Ok(out)
    }

    pub fn finalize_scan(&self, roots: &[String], generation: u64) -> Result<Finalized> {
        let under = |p: &str| {
            roots.iter().any(|r| {
                p == r
                    || p.strip_prefix(r.as_str())
                        .is_some_and(|x| x.starts_with('/'))
            })
        };
        let mut stale: Vec<(String, bool)> = Vec::new();
        let mut sizes: HashMap<String, u64> = HashMap::new();
        let mut seen_inodes: HashSet<u64> = HashSet::new();
        {
            let r = self.db.begin_read().map_err(db_err)?;
            let files = r.open_table(FILES).map_err(db_err)?;
            for row in files.iter().map_err(db_err)? {
                let (k, v) = row.map_err(db_err)?;
                let path = k.value();
                let Some(rec) = Record::from_bytes(v.value()) else {
                    continue;
                };
                if rec.generation < generation && under(path) {
                    stale.push((path.to_string(), rec.is_dir));
                    continue;
                }
                if rec.is_dir {
                    sizes.entry(path.to_string()).or_insert(0);
                } else if seen_inodes.insert(rec.inode) {
                    let mut dir = path;
                    while let Some(i) = dir.rfind('/') {
                        dir = &dir[..i];
                        if dir.is_empty() {
                            break;
                        }
                        match sizes.get_mut(dir) {
                            Some(total) => *total += rec.size,
                            None => {
                                sizes.insert(dir.to_string(), rec.size);
                            }
                        }
                    }
                }
            }
        }
        let w = self.db.begin_write().map_err(db_err)?;
        {
            let mut files = w.open_table(FILES).map_err(db_err)?;
            let mut names = w.open_table(NAME_PATH).map_err(db_err)?;
            let mut exts = w.open_table(EXT_NAME).map_err(db_err)?;
            for (p, is_dir) in &stale {
                files.remove(p.as_str()).map_err(db_err)?;
                names.remove(name_key(p).as_str()).map_err(db_err)?;
                if !is_dir {
                    exts.remove(ext_key(p).as_str()).map_err(db_err)?;
                }
            }
        }
        w.delete_table(FOLDER_SIZES).map_err(db_err)?;
        {
            let mut table = w.open_table(FOLDER_SIZES).map_err(db_err)?;
            for (dir, size) in &sizes {
                table.insert(dir.as_str(), *size).map_err(db_err)?;
            }
        }
        w.commit().map_err(db_err)?;
        Ok(Finalized {
            stale_removed: stale.len() as u64,
            folders: sizes.len() as u64,
        })
    }

    pub fn folder_size(&self, dir: &str) -> Result<Option<u64>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(FOLDER_SIZES).map_err(db_err)?;
        Ok(t.get(dir).map_err(db_err)?.map(|g| g.value()))
    }

    pub fn get(&self, path: &str) -> Result<Option<Record>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let files = r.open_table(FILES).map_err(db_err)?;
        let got = files.get(path).map_err(db_err)?;
        Ok(got.and_then(|g| Record::from_bytes(g.value())))
    }

    pub fn count(&self) -> Result<u64> {
        let r = self.db.begin_read().map_err(db_err)?;
        r.open_table(FILES).map_err(db_err)?.len().map_err(db_err)
    }

    pub fn remove_tree(&self, path: &str) -> Result<u64> {
        let (from, to) = after_prefix(path);
        let w = self.db.begin_write().map_err(db_err)?;
        let removed;
        {
            let mut files = w.open_table(FILES).map_err(db_err)?;
            let mut names = w.open_table(NAME_PATH).map_err(db_err)?;
            let mut exts = w.open_table(EXT_NAME).map_err(db_err)?;
            let mut sizes = w.open_table(FOLDER_SIZES).map_err(db_err)?;
            let mut doomed: Vec<(String, bool)> = Vec::new();
            for row in files.range(from.as_str()..to.as_str()).map_err(db_err)? {
                let (k, v) = row.map_err(db_err)?;
                let is_dir = Record::from_bytes(v.value()).is_some_and(|r| r.is_dir);
                doomed.push((k.value().to_string(), is_dir));
            }
            if let Some(g) = files.get(path).map_err(db_err)? {
                let is_dir = Record::from_bytes(g.value()).is_some_and(|r| r.is_dir);
                doomed.push((path.to_string(), is_dir));
            }
            removed = doomed.len() as u64;
            for (p, is_dir) in &doomed {
                files.remove(p.as_str()).map_err(db_err)?;
                names.remove(name_key(p).as_str()).map_err(db_err)?;
                if *is_dir {
                    sizes.remove(p.as_str()).map_err(db_err)?;
                } else {
                    exts.remove(ext_key(p).as_str()).map_err(db_err)?;
                }
            }
        }
        w.commit().map_err(db_err)?;
        Ok(removed)
    }

    pub fn add_exclusion(&self, pattern: &str) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(EXCLUSIONS)
            .map_err(db_err)?
            .insert(pattern, ())
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    pub fn remove_exclusion(&self, pattern: &str) -> Result<bool> {
        let w = self.db.begin_write().map_err(db_err)?;
        let had = w
            .open_table(EXCLUSIONS)
            .map_err(db_err)?
            .remove(pattern)
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        Ok(had)
    }

    pub fn exclusions(&self) -> Result<Vec<String>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(EXCLUSIONS).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(db_err)? {
            out.push(row.map_err(db_err)?.0.value().to_string());
        }
        Ok(out)
    }

    pub fn meta(&self, key: &str) -> Result<Option<u64>> {
        let r = self.db.begin_read().map_err(db_err)?;
        let t = r.open_table(META).map_err(db_err)?;
        Ok(t.get(key).map_err(db_err)?.map(|g| g.value()))
    }

    pub fn set_meta(&self, key: &str, value: u64) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(META)
            .map_err(db_err)?
            .insert(key, value)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }

    pub fn remove_meta(&self, key: &str) -> Result<()> {
        let w = self.db.begin_write().map_err(db_err)?;
        w.open_table(META)
            .map_err(db_err)?
            .remove(key)
            .map_err(db_err)?;
        w.commit().map_err(db_err)
    }
}

#[cfg(test)]
pub fn temp_index(tag: &str) -> (std::path::PathBuf, Index) {
    let dir = std::env::temp_dir().join(format!("au-fi-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let index = Index::open(&dir.join("index.redb")).unwrap();
    (dir, index)
}

#[cfg(test)]
pub fn test_tree_root(tag: &str) -> std::path::PathBuf {
    std::path::Path::new("/var/tmp/au-fi-trees").join(format!("{}-{tag}", std::process::id()))
}

#[cfg(test)]
pub fn entry(path: &str, size: u64, is_dir: bool) -> Entry {
    Entry {
        path: path.to_string(),
        record: Record {
            size,
            mtime: 1,
            inode: 1,
            is_dir,
            generation: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_roundtrip() {
        let r = Record {
            size: 5,
            mtime: 6,
            inode: 7,
            is_dir: true,
            generation: 8,
        };
        assert_eq!(Record::from_bytes(&r.to_bytes()), Some(r));
        assert_eq!(Record::from_bytes(&[0; 3]), None);
    }

    #[test]
    fn names_and_extensions() {
        assert_eq!(name_of("/a/b/Report.PDF"), "Report.PDF");
        assert_eq!(ext_of("/a/b/Report.PDF"), "pdf");
        assert_eq!(ext_of("/a/.bashrc"), "");
        assert_eq!(ext_of("/a/Makefile"), "");
        assert_eq!(ext_of("/a/x.tar.gz"), "gz");
    }

    #[test]
    fn upsert_get_count() {
        let (dir, ix) = temp_index("upsert");
        ix.upsert(&[entry("/a/x.txt", 3, false), entry("/a", 0, true)])
            .unwrap();
        assert_eq!(ix.count().unwrap(), 2);
        assert_eq!(ix.get("/a/x.txt").unwrap().unwrap().size, 3);
        ix.upsert(&[entry("/a/x.txt", 9, false)]).unwrap();
        assert_eq!(ix.count().unwrap(), 2);
        assert_eq!(ix.get("/a/x.txt").unwrap().unwrap().size, 9);
        assert_eq!(ix.get("/nope").unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_tree_takes_descendants_but_not_siblings_with_the_same_prefix() {
        let (dir, ix) = temp_index("remove");
        ix.upsert(&[
            entry("/a", 0, true),
            entry("/a/x.txt", 1, false),
            entry("/a/sub/y.txt", 1, false),
            entry("/ab/z.txt", 1, false),
        ])
        .unwrap();
        assert_eq!(ix.remove_tree("/a").unwrap(), 3);
        assert_eq!(ix.count().unwrap(), 1);
        assert!(ix.get("/ab/z.txt").unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn frontier_is_replaced_per_job_and_survives_reopen() {
        let (dir, ix) = temp_index("frontier");
        ix.commit_scan(
            "j1",
            &[entry("/a/x", 1, false)],
            &["/a".into(), "/b".into()],
        )
        .unwrap();
        ix.commit_scan("j2", &[], &["/z".into()]).unwrap();
        ix.commit_scan("j1", &[], &["/c".into()]).unwrap();
        drop(ix);
        let ix = Index::open(&dir.join("index.redb")).unwrap();
        assert_eq!(ix.frontier("j1").unwrap(), ["/c"]);
        assert_eq!(ix.frontier("j2").unwrap(), ["/z"]);
        assert_eq!(ix.count().unwrap(), 1);
        ix.clear_frontier("j1").unwrap();
        assert!(ix.frontier("j1").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn rec(path: &str, size: u64, is_dir: bool, inode: u64, generation: u64) -> Entry {
        Entry {
            path: path.to_string(),
            record: Record {
                size,
                mtime: 1,
                inode,
                is_dir,
                generation,
            },
        }
    }

    #[test]
    fn finalize_drops_stale_records_and_sizes_folders_without_double_counting_hardlinks() {
        let (dir, ix) = temp_index("finalize");
        ix.upsert(&[
            rec("/r", 0, true, 1, 1),
            rec("/r/a.txt", 10, false, 2, 1),
            rec("/r/sub", 0, true, 3, 1),
            rec("/r/sub/b.txt", 5, false, 4, 1),
            rec("/r/sub/link.txt", 5, false, 4, 1),
            rec("/r/old.txt", 100, false, 5, 0),
            rec("/elsewhere/keep.txt", 7, false, 6, 0),
        ])
        .unwrap();
        let f = ix.finalize_scan(&["/r".to_string()], 1).unwrap();
        assert_eq!(f.stale_removed, 1);
        assert_eq!(ix.get("/r/old.txt").unwrap(), None);
        assert!(ix.get("/elsewhere/keep.txt").unwrap().is_some());
        assert_eq!(ix.folder_size("/r").unwrap(), Some(15));
        assert_eq!(ix.folder_size("/r/sub").unwrap(), Some(5));
        assert_eq!(ix.folder_size("/nope").unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jobs_are_stored_and_listed() {
        let (dir, ix) = temp_index("jobs");
        ix.save_job("j1", b"one").unwrap();
        ix.save_job("j2", b"two").unwrap();
        assert_eq!(ix.load_job("j1").unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(ix.load_job("nope").unwrap(), None);
        assert_eq!(ix.list_jobs().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exclusions_and_meta_persist_across_reopen() {
        let (dir, ix) = temp_index("persist");
        ix.add_exclusion("target/").unwrap();
        ix.set_meta("generation", 4).unwrap();
        drop(ix);
        let ix = Index::open(&dir.join("index.redb")).unwrap();
        assert_eq!(ix.exclusions().unwrap(), ["target/"]);
        assert_eq!(ix.meta("generation").unwrap(), Some(4));
        assert!(ix.remove_exclusion("target/").unwrap());
        assert!(!ix.remove_exclusion("target/").unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
