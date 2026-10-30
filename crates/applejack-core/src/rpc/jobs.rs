//! Long-running work: a method returns a [`JobStarted`] and the core follows
//! up with `job.progress` notifications and one `job.done` or `job.failed`.

use super::wire::{ErrorCode, Notification};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

/// The immediate result of a method that runs as a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStarted {
    pub job_id: JobId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobProgress {
    pub job_id: JobId,
    pub phase: String,
    pub done_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDone {
    pub job_id: JobId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFailed {
    pub job_id: JobId,
    pub code: ErrorCode,
    pub message: String,
}

impl JobProgress {
    pub const METHOD: &'static str = "job.progress";
}

impl JobDone {
    pub const METHOD: &'static str = "job.done";
}

impl JobFailed {
    pub const METHOD: &'static str = "job.failed";
}

fn notification<T: Serialize>(method: &str, params: &T) -> Notification {
    // The job types are plain data; serializing them cannot fail.
    Notification::new(method, serde_json::to_value(params).unwrap_or(Value::Null))
}

impl From<&JobProgress> for Notification {
    fn from(progress: &JobProgress) -> Self {
        notification(JobProgress::METHOD, progress)
    }
}

impl From<&JobDone> for Notification {
    fn from(done: &JobDone) -> Self {
        notification(JobDone::METHOD, done)
    }
}

impl From<&JobFailed> for Notification {
    fn from(failed: &JobFailed) -> Self {
        notification(JobFailed::METHOD, failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_failed_becomes_a_notification_with_an_integer_code() {
        let failed = JobFailed {
            job_id: JobId(7),
            code: ErrorCode::JobFailed,
            message: "disk full".into(),
        };
        let notification = Notification::from(&failed);
        assert_eq!(
            serde_json::to_value(&notification).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "method": "job.failed",
                "params": {"job_id": 7, "code": 1004, "message": "disk full"}
            })
        );
    }

    #[test]
    fn job_progress_round_trips() {
        let progress = JobProgress {
            job_id: JobId(1),
            phase: "download".into(),
            done_bytes: Some(10),
            total_bytes: None,
            message: String::new(),
        };
        let value = serde_json::to_value(&progress).unwrap();
        assert_eq!(value["total_bytes"], Value::Null);
        assert_eq!(
            serde_json::from_value::<JobProgress>(value).unwrap(),
            progress
        );
    }
}
