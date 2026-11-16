use serde::{Deserialize, Serialize};

pub const RUN_COMMAND: &str = "run-command";
pub const JOB_PROGRESS: &str = "job-progress";
pub const JOB_CANCEL: &str = "job-cancel";
pub const FILES_CHANGED: &str = "files-changed";
pub const FILE_LOOKUP: &str = "file-lookup";
pub const SEARCH: &str = "search";
pub const WATCH_SET_ROOTS: &str = "watch-set-roots";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommand {
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutput {
    pub text: String,
    #[serde(default)]
    pub job: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    pub job: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Paused,
    Done,
    Cancelled,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    pub job: String,
    pub state: JobState,
    #[serde(default)]
    pub percent: Option<u8>,
    #[serde(default)]
    pub eta_secs: Option<u64>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CancelMode {
    Force,
    Safe,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancel {
    pub job: String,
    pub mode: CancelMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesChanged {
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seq: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub first_seq: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLookup {
    #[serde(default)]
    pub name_contains: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub under: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLookupResult {
    pub files: Vec<FileInfo>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSetRoots {
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Search {
    pub pattern: String,
    #[serde(default)]
    pub under: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub files: Vec<FileInfo>,
    #[serde(default)]
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;

    #[test]
    fn standard_messages_roundtrip() {
        let c = Envelope::call(
            JOB_CANCEL,
            &JobCancel {
                job: "7f3a9c".into(),
                mode: CancelMode::Safe,
            },
        )
        .unwrap();
        let back: JobCancel = Envelope::decode(&c.encode().unwrap())
            .unwrap()
            .body_as()
            .unwrap();
        assert_eq!(back.mode, CancelMode::Safe);
    }

    #[test]
    fn states_and_modes_from_the_future_decode_as_unknown() {
        #[derive(Serialize)]
        struct NewStatus {
            job: String,
            state: String,
        }
        let e = Envelope::reply(
            &Envelope::call(JOB_PROGRESS, &()).unwrap(),
            &NewStatus {
                job: "7f3a9c".into(),
                state: "hibernating".into(),
            },
        )
        .unwrap();
        let s: JobStatus = e.body_as().unwrap();
        assert_eq!(
            (s.state, s.percent, s.detail),
            (JobState::Unknown, None, None)
        );

        #[derive(Serialize)]
        struct NewCancel {
            job: String,
            mode: String,
        }
        let e = Envelope::call(
            JOB_CANCEL,
            &NewCancel {
                job: "7f3a9c".into(),
                mode: "gentle".into(),
            },
        )
        .unwrap();
        assert_eq!(e.body_as::<JobCancel>().unwrap().mode, CancelMode::Unknown);
    }

    #[test]
    fn known_states_and_modes_still_use_their_plain_names() {
        for (state, name) in [
            (JobState::Running, "running"),
            (JobState::Paused, "paused"),
            (JobState::Done, "done"),
            (JobState::Cancelled, "cancelled"),
            (JobState::Failed, "failed"),
        ] {
            let bytes = rmp_serde::to_vec_named(&state).unwrap();
            assert_eq!(rmp_serde::from_slice::<String>(&bytes).unwrap(), name);
        }
        for (mode, name) in [(CancelMode::Force, "force"), (CancelMode::Safe, "safe")] {
            let bytes = rmp_serde::to_vec_named(&mode).unwrap();
            assert_eq!(rmp_serde::from_slice::<String>(&bytes).unwrap(), name);
        }
    }

    #[test]
    fn optional_boxes_may_be_missing() {
        #[derive(Serialize)]
        struct Old {
            text: String,
        }
        let e = Envelope::call(RUN_COMMAND, &Old { text: "hi".into() }).unwrap();
        let out: CommandOutput = e.body_as().unwrap();
        assert_eq!(out.job, None);
    }

    #[test]
    fn progress_is_only_what_was_asked_for() {
        let s = JobStatus {
            job: "7f3a9c".into(),
            state: JobState::Running,
            percent: Some(50),
            eta_secs: None,
            detail: None,
        };
        let e = Envelope::reply(
            &Envelope::call(JOB_PROGRESS, &JobProgress { job: s.job.clone() }).unwrap(),
            &s,
        )
        .unwrap();
        assert_eq!(e.body_as::<JobStatus>().unwrap(), s);
    }

    #[test]
    fn a_lookup_with_no_boxes_means_everything() {
        let e = Envelope::call(FILE_LOOKUP, &()).unwrap();
        let q: FileLookup = e.body_as().unwrap_or_default();
        assert_eq!(q, FileLookup::default());
    }

    #[test]
    fn files_changed_batches_many_paths() {
        let batch = FilesChanged {
            changed: (0..50_000).map(|i| format!("/a/{i}")).collect(),
            removed: vec![],
            ..Default::default()
        };
        let e = Envelope::event(FILES_CHANGED, &batch).unwrap();
        let back: FilesChanged = Envelope::decode(&e.encode().unwrap())
            .unwrap()
            .body_as()
            .unwrap();
        assert_eq!(back.changed.len(), 50_000);
    }
}
