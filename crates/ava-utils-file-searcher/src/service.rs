use ava_utils_config::error::{Error, Result};
use ava_utils_config::flags::{Spec, parse};
use ava_utils_config::messages::{FileLookup, FileLookupResult, Search, SearchResult};

use crate::glob::Glob;

pub const DEFAULT_LIMIT: u32 = 100;
pub const INDEX_PAGE: u32 = 10_000;

pub fn search(
    lookup: &mut dyn FnMut(&FileLookup) -> Result<FileLookupResult>,
    req: &Search,
) -> Result<SearchResult> {
    if req.pattern.is_empty() {
        return Err(Error::Usage("empty search pattern".into()));
    }
    let glob = Glob::new(&req.pattern);
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).max(1) as usize;
    let q = FileLookup {
        ext: glob.literal_ext(),
        name_contains: if glob.literal_ext().is_some() {
            None
        } else {
            glob.longest_literal()
        },
        under: req.under.clone(),
        limit: Some(INDEX_PAGE),
    };
    let found = lookup(&q)?;
    let mut files: Vec<_> = found
        .files
        .into_iter()
        .filter(|f| glob.matches(&f.path))
        .collect();
    let more = files.len() > limit;
    files.truncate(limit);
    Ok(SearchResult {
        files,
        truncated: more || found.truncated,
    })
}

pub fn run_command(
    lookup: &mut dyn FnMut(&FileLookup) -> Result<FileLookupResult>,
    args: &[String],
) -> Result<String> {
    match args.first().map(String::as_str) {
        None => Ok(
            "file-searcher is up. commands: find <pattern> [--under <folder>] [--limit <n>]".into(),
        ),
        Some("find") => {
            let a = parse(&Spec::new().option("under").option("limit"), &args[1..])?;
            let pattern = a.positional().first().ok_or_else(|| {
                Error::Usage("usage: find <pattern> [--under <folder>] [--limit <n>]".into())
            })?;
            let req = Search {
                pattern: pattern.clone(),
                under: a.get("under").map(str::to_string),
                limit: a.get_parsed("limit")?,
            };
            let r = search(lookup, &req)?;
            let mut out: Vec<String> = r.files.iter().map(|f| f.path.clone()).collect();
            if r.truncated {
                out.push("(more results exist: use --limit or --under to narrow)".into());
            } else if out.is_empty() {
                out.push("no matches".into());
            }
            Ok(out.join("\n"))
        }
        Some(other) => Err(Error::Usage(format!("unknown command: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ava_utils_config::messages::FileInfo;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn fi(p: &str) -> FileInfo {
        FileInfo {
            path: p.into(),
            size: 1,
            mtime: 1,
            is_dir: false,
        }
    }

    fn fake(
        files: &[&str],
        truncated: bool,
        seen: &Rc<RefCell<Vec<FileLookup>>>,
    ) -> impl FnMut(&FileLookup) -> Result<FileLookupResult> {
        let files: Vec<FileInfo> = files.iter().map(|p| fi(p)).collect();
        let seen = seen.clone();
        move |q: &FileLookup| {
            seen.borrow_mut().push(q.clone());
            Ok(FileLookupResult {
                files: files.clone(),
                truncated,
            })
        }
    }

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn extension_patterns_ask_the_indexer_by_extension() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut l = fake(&["/a/x.rs", "/a/y.rs"], false, &seen);
        let r = search(
            &mut l,
            &Search {
                pattern: "*.rs".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.files.len(), 2);
        assert_eq!(seen.borrow()[0].ext.as_deref(), Some("rs"));
        assert_eq!(seen.borrow()[0].limit, Some(INDEX_PAGE));
    }

    #[test]
    fn results_are_filtered_by_the_real_pattern() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut l = fake(
            &["/a/report-final.txt", "/a/report.pdf", "/a/other-final.txt"],
            false,
            &seen,
        );
        let r = search(
            &mut l,
            &Search {
                pattern: "report*.txt".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(seen.borrow()[0].name_contains.as_deref(), Some("report"));
    }

    #[test]
    fn limit_and_truncation_are_reported() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut l = fake(&["/a/1.rs", "/a/2.rs", "/a/3.rs"], false, &seen);
        let r = search(
            &mut l,
            &Search {
                pattern: "*.rs".into(),
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((r.files.len(), r.truncated), (2, true));
        let mut l = fake(&["/a/1.rs"], true, &seen);
        assert!(
            search(
                &mut l,
                &Search {
                    pattern: "*.rs".into(),
                    ..Default::default()
                }
            )
            .unwrap()
            .truncated
        );
    }

    #[test]
    fn indexer_errors_pass_through_and_empty_patterns_are_refused() {
        let mut down = |_: &FileLookup| -> Result<FileLookupResult> { Err(Error::Unavailable) };
        assert!(matches!(
            search(
                &mut down,
                &Search {
                    pattern: "*.rs".into(),
                    ..Default::default()
                }
            ),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            search(&mut down, &Search::default()),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn the_find_command_prints_paths_and_passes_flags() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut l = fake(&["/a/x.rs"], false, &seen);
        assert!(run_command(&mut l, &[]).unwrap().contains("is up"));
        let out = run_command(
            &mut l,
            &a(&["find", "*.rs", "--under", "/a", "--limit", "5"]),
        )
        .unwrap();
        assert_eq!(out, "/a/x.rs");
        assert_eq!(seen.borrow()[0].under.as_deref(), Some("/a"));
        assert!(matches!(
            run_command(&mut l, &a(&["find"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            run_command(&mut l, &a(&["find", "x", "--limit", "no"])),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            run_command(&mut l, &a(&["wat"])),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn no_matches_says_so() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut l = fake(&[], false, &seen);
        assert_eq!(
            run_command(&mut l, &a(&["find", "*.zzz"])).unwrap(),
            "no matches"
        );
    }
}
