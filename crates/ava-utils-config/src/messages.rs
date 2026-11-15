use serde::{Deserialize, Serialize};

pub const RUN_COMMAND: &str = "run-command";
pub const JOB_PROGRESS: &str = "job-progress";
pub const JOB_CANCEL: &str = "job-cancel";

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
    Done,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    pub job: String,
    pub state: JobState,
    #[serde(default)]
    pub percent: Option<u8>,
    #[serde(default)]
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CancelMode {
    Force,
    Safe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCancel {
    pub job: String,
    pub mode: CancelMode,
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
        };
        let e = Envelope::reply(
            &Envelope::call(JOB_PROGRESS, &JobProgress { job: s.job.clone() }).unwrap(),
            &s,
        )
        .unwrap();
        assert_eq!(e.body_as::<JobStatus>().unwrap(), s);
    }
}
