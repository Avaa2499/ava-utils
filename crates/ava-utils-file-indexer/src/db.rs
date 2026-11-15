use std::path::Path;

use ava_utils_config::error::{Error, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

pub const SCHEMA_VERSION: u64 = 1;

const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
const EXT_NAME: TableDefinition<&str, ()> = TableDefinition::new("ext_name");
const NAME_PATH: TableDefinition<&str, ()> = TableDefinition::new("name_path");
const FOLDER_SIZES: TableDefinition<&str, u64> = TableDefinition::new("folder_sizes");
const EXCLUSIONS: TableDefinition<&str, ()> = TableDefinition::new("exclusions");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

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

fn name_key(path: &str) -> String {
    format!("{}\0{path}", name_of(path).to_lowercase())
}

fn ext_key(path: &str) -> String {
    format!("{}\0{}\0{path}", ext_of(path), name_of(path).to_lowercase())
}

fn after_prefix(dir: &str) -> (String, String) {
    (format!("{dir}/"), format!("{dir}0"))
}

pub struct Index {
    db: Database,
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
}

#[cfg(test)]
pub fn temp_index(tag: &str) -> (std::path::PathBuf, Index) {
    let dir = std::env::temp_dir().join(format!("au-fi-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let index = Index::open(&dir.join("index.redb")).unwrap();
    (dir, index)
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
