use ava_utils_config::error::Result;
use ava_utils_config::messages::{FileInfo, FileLookup, FileLookupResult};
use redb::{ReadableDatabase, ReadableTable};

use crate::db::{EXT_NAME, FILES, Index, NAME_PATH, Record, db_err};

pub const DEFAULT_LIMIT: u32 = 100;
pub const MAX_LIMIT: u32 = 10_000;

fn under_ok(path: &str, under: &Option<String>) -> bool {
    match under {
        None => true,
        Some(u) => {
            path == u
                || path
                    .strip_prefix(u.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        }
    }
}

impl Index {
    pub fn lookup(&self, q: &FileLookup) -> Result<FileLookupResult> {
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
        let needle = q.name_contains.as_ref().map(|n| n.to_lowercase());
        let under = q
            .under
            .as_ref()
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty());
        let r = self.db.begin_read().map_err(db_err)?;
        let files = r.open_table(FILES).map_err(db_err)?;
        let mut paths: Vec<String> = Vec::new();

        let mut take = |name: &str, path: &str| -> bool {
            if needle.as_ref().is_some_and(|n| !name.contains(n.as_str())) {
                return true;
            }
            if !under_ok(path, &under) {
                return true;
            }
            paths.push(path.to_string());
            paths.len() <= limit
        };

        if let Some(ext) = &q.ext {
            let ext = ext.trim_start_matches('.').to_lowercase();
            let exts = r.open_table(EXT_NAME).map_err(db_err)?;
            let (from, to) = (format!("{ext}\0"), format!("{ext}\u{1}"));
            for row in exts.range(from.as_str()..to.as_str()).map_err(db_err)? {
                let k = row.map_err(db_err)?.0;
                let mut parts = k.value().splitn(3, '\0');
                let (_, name, path) = (parts.next(), parts.next(), parts.next());
                if let (Some(name), Some(path)) = (name, path)
                    && !take(name, path)
                {
                    break;
                }
            }
        } else if needle.is_some() {
            let names = r.open_table(NAME_PATH).map_err(db_err)?;
            for row in names.iter().map_err(db_err)? {
                let k = row.map_err(db_err)?.0;
                if let Some((name, path)) = k.value().split_once('\0')
                    && !take(name, path)
                {
                    break;
                }
            }
        } else {
            let range = match &under {
                Some(u) => (format!("{u}/"), format!("{u}0")),
                None => (String::new(), "\u{10ffff}".to_string()),
            };
            for row in files
                .range(range.0.as_str()..range.1.as_str())
                .map_err(db_err)?
            {
                let k = row.map_err(db_err)?.0;
                if !take("", k.value()) {
                    break;
                }
            }
        }

        let truncated = paths.len() > limit;
        paths.truncate(limit);
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(rec) = files
                .get(path.as_str())
                .map_err(db_err)?
                .and_then(|g| Record::from_bytes(g.value()))
            {
                out.push(FileInfo {
                    path,
                    size: rec.size,
                    mtime: rec.mtime,
                    is_dir: rec.is_dir,
                });
            }
        }
        Ok(FileLookupResult {
            files: out,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{entry, temp_index};

    fn paths(r: &FileLookupResult) -> Vec<&str> {
        let mut p: Vec<&str> = r.files.iter().map(|f| f.path.as_str()).collect();
        p.sort();
        p
    }

    fn sample() -> (std::path::PathBuf, Index) {
        let (dir, ix) = temp_index(&format!("lookup-{:?}", std::thread::current().id()));
        ix.upsert(&[
            entry("/home/a", 0, true),
            entry("/home/a/Report.PDF", 10, false),
            entry("/home/a/notes.txt", 20, false),
            entry("/home/a/sub/report-old.pdf", 30, false),
            entry("/home/ab/report.pdf", 40, false),
        ])
        .unwrap();
        (dir, ix)
    }

    #[test]
    fn by_extension_ignores_case_and_a_leading_dot() {
        let (dir, ix) = sample();
        let q = FileLookup {
            ext: Some(".PDF".into()),
            ..Default::default()
        };
        assert_eq!(paths(&ix.lookup(&q).unwrap()).len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn by_name_substring_finds_files_and_folders() {
        let (dir, ix) = sample();
        let q = FileLookup {
            name_contains: Some("REPORT".into()),
            ..Default::default()
        };
        assert_eq!(ix.lookup(&q).unwrap().files.len(), 3);
        let q = FileLookup {
            name_contains: Some("a".into()),
            under: Some("/home/a/".into()),
            ..Default::default()
        };
        assert!(
            paths(&ix.lookup(&q).unwrap())
                .iter()
                .all(|p| *p == "/home/a" || p.starts_with("/home/a/"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn under_does_not_match_sibling_folders_with_the_same_prefix() {
        let (dir, ix) = sample();
        let q = FileLookup {
            under: Some("/home/a".into()),
            ..Default::default()
        };
        let got = ix.lookup(&q).unwrap();
        assert!(!paths(&got).contains(&"/home/ab/report.pdf"));
        assert_eq!(got.files.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn combined_filters_and_limit() {
        let (dir, ix) = sample();
        let q = FileLookup {
            ext: Some("pdf".into()),
            name_contains: Some("old".into()),
            ..Default::default()
        };
        assert_eq!(
            paths(&ix.lookup(&q).unwrap()),
            ["/home/a/sub/report-old.pdf"]
        );
        let q = FileLookup {
            limit: Some(2),
            ..Default::default()
        };
        let got = ix.lookup(&q).unwrap();
        assert_eq!(got.files.len(), 2);
        assert!(got.truncated);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn results_carry_size_and_kind() {
        let (dir, ix) = sample();
        let q = FileLookup {
            name_contains: Some("notes".into()),
            ..Default::default()
        };
        let f = &ix.lookup(&q).unwrap().files[0];
        assert_eq!((f.size, f.is_dir), (20, false));
        let _ = std::fs::remove_dir_all(dir);
    }
}
