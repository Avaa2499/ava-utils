use std::path::Path;
use std::sync::Arc;

use ava_utils_config::error::{Error, Result};
use ava_utils_config::flags::{Spec, parse};
use ava_utils_config::messages::{
    CancelMode, CommandOutput, FileLookup, FileLookupResult, FilesChanged, JobCancel, JobProgress,
    JobStatus,
};

use crate::apply::apply_changes;
use crate::db::Index;
use crate::exclude::DEFAULT_EXCLUSIONS;
use crate::jobs::Jobs;

pub struct Service {
    pub index: Arc<Index>,
    pub jobs: Jobs,
}

fn usage(s: &str) -> Error {
    Error::Usage(s.to_string())
}

impl Service {
    pub fn open(db_path: &Path, workers: usize) -> Result<Self> {
        let index = Arc::new(Index::open(db_path)?);
        if index.exclusions()?.is_empty() {
            for p in DEFAULT_EXCLUSIONS {
                index.add_exclusion(p)?;
            }
        }
        let jobs = Jobs::new(index.clone(), workers)?;
        Ok(Self { index, jobs })
    }

    pub fn run_command(&self, args: &[String]) -> Result<CommandOutput> {
        let say = |text: String| CommandOutput { text, job: None };
        let Some(cmd) = args.first() else {
            return Ok(say(format!(
                "file-indexer is up. {} entries indexed. commands: scan, status, progress, cancel, resume, exclude",
                self.index.count()?
            )));
        };
        let rest = &args[1..];
        match cmd.as_str() {
            "scan" => {
                if rest.is_empty() {
                    return Err(usage("usage: scan <folder>..."));
                }
                let id = self.jobs.start_scan(rest.to_vec())?;
                Ok(CommandOutput {
                    text: format!("scan started, job {id}"),
                    job: Some(id),
                })
            }
            "resume" => {
                let id = self
                    .jobs
                    .resume(rest.first().ok_or_else(|| usage("usage: resume <job>"))?)?;
                Ok(CommandOutput {
                    text: format!("job {id} resumed"),
                    job: Some(id),
                })
            }
            "progress" => {
                let id = rest.first().ok_or_else(|| usage("usage: progress <job>"))?;
                Ok(say(format_status(&self.jobs.status(id)?)))
            }
            "status" => Ok(say(match self.jobs.latest()? {
                Some(r) => format!(
                    "{} entries indexed. latest job: {}",
                    self.index.count()?,
                    format_status(&self.jobs.status(&r.id)?)
                ),
                None => format!("{} entries indexed. no jobs yet", self.index.count()?),
            })),
            "cancel" => {
                let a = parse(&Spec::new().switch("safe").switch("force"), rest)?;
                let id = a
                    .positional()
                    .first()
                    .ok_or_else(|| usage("usage: cancel <job> [--safe|--force]"))?;
                if a.has("safe") && a.has("force") {
                    return Err(usage("pick one: --safe or --force"));
                }
                let mode = if a.has("force") {
                    CancelMode::Force
                } else {
                    CancelMode::Safe
                };
                self.jobs.cancel(id, mode)?;
                Ok(say(format!("job {id}: {mode:?} cancel requested")))
            }
            "exclude" => match rest.first().map(String::as_str) {
                Some("add") => {
                    let p = rest
                        .get(1)
                        .ok_or_else(|| usage("usage: exclude add <pattern>"))?;
                    self.index.add_exclusion(p)?;
                    Ok(say(format!("excluded {p} (applies to the next scan)")))
                }
                Some("remove") => {
                    let p = rest
                        .get(1)
                        .ok_or_else(|| usage("usage: exclude remove <pattern>"))?;
                    self.index.remove_exclusion(p)?;
                    Ok(say(format!("no longer excluding {p}")))
                }
                Some("list") | None => Ok(say(self.index.exclusions()?.join("\n"))),
                Some(other) => Err(Error::Usage(format!("unknown exclude command: {other}"))),
            },
            other => Err(Error::Usage(format!("unknown command: {other}"))),
        }
    }

    pub fn job_progress(&self, req: &JobProgress) -> Result<JobStatus> {
        self.jobs.status(&req.job)
    }

    pub fn job_cancel(&self, req: &JobCancel) -> Result<()> {
        self.jobs.cancel(&req.job, req.mode)
    }

    pub fn file_lookup(&self, q: &FileLookup) -> Result<FileLookupResult> {
        self.index.lookup(q)
    }

    pub fn files_changed(&self, batch: &FilesChanged) -> Result<()> {
        apply_changes(&self.index, batch).map(|_| ())
    }
}

fn format_status(s: &JobStatus) -> String {
    format!(
        "job {}: {:?}{}",
        s.job,
        s.state,
        s.detail
            .as_ref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{temp_index, test_tree_root};
    use ava_utils_config::messages::JobState;
    use std::time::{Duration, Instant};

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn svc(tag: &str) -> (std::path::PathBuf, Service, std::path::PathBuf) {
        let (dir, ix) = temp_index(tag);
        drop(ix);
        let root = test_tree_root(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.txt"), b"hi").unwrap();
        let s = Service::open(&dir.join("index.redb"), 2).unwrap();
        (dir, s, root)
    }

    #[test]
    fn no_arguments_says_it_is_up() {
        let (dir, s, root) = svc("svc-up");
        assert!(s.run_command(&[]).unwrap().text.contains("is up"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn default_exclusions_are_seeded_once() {
        let (dir, s, root) = svc("svc-excl");
        assert!(
            s.run_command(&a(&["exclude", "list"]))
                .unwrap()
                .text
                .contains("node_modules/")
        );
        s.run_command(&a(&["exclude", "remove", "node_modules/"]))
            .unwrap();
        drop(s);
        let s = Service::open(&dir.join("index.redb"), 1).unwrap();
        assert!(
            s.index
                .exclusions()
                .unwrap()
                .contains(&"target/".to_string())
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_needs_a_folder_and_progress_reports_it() {
        let (dir, s, root) = svc("svc-scan");
        assert!(matches!(s.run_command(&a(&["scan"])), Err(Error::Usage(_))));
        let out = s
            .run_command(&a(&["scan", &root.to_string_lossy()]))
            .unwrap();
        let id = out.job.unwrap();
        let t = Instant::now();
        loop {
            let st = s.job_progress(&JobProgress { job: id.clone() }).unwrap();
            if st.state == JobState::Done {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        let found = s
            .file_lookup(&FileLookup {
                name_contains: Some("a.txt".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(found.files.len(), 1);
        assert!(
            s.run_command(&a(&["status"]))
                .unwrap()
                .text
                .contains("latest job")
        );
        assert!(
            s.run_command(&a(&["progress", &id]))
                .unwrap()
                .text
                .contains("Done")
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bad_commands_are_usage_errors() {
        let (dir, s, root) = svc("svc-bad");
        for args in [
            &["nope"][..],
            &["resume"],
            &["progress"],
            &["cancel"],
            &["cancel", "x", "--safe", "--force"],
            &["exclude", "wat"],
        ] {
            assert!(
                matches!(s.run_command(&a(args)), Err(Error::Usage(_))),
                "{args:?}"
            );
        }
        assert!(s.run_command(&a(&["cancel", "zzzzzz"])).is_err());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn files_changed_updates_the_index() {
        let (dir, s, root) = svc("svc-changed");
        let f = root.join("sub/a.txt").to_string_lossy().into_owned();
        s.files_changed(&FilesChanged {
            changed: vec![f.clone()],
            removed: vec![],
            ..Default::default()
        })
        .unwrap();
        assert!(s.index.get(&f).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }
}
