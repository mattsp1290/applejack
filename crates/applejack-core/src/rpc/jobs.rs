//! Long-running work: a method returns a [`JobStarted`] and the core follows
//! up with `job.progress` notifications and one `job.done` or `job.failed`.

use super::wire::{ErrorCode, Notification, Response, RpcError};
use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

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

/// The one way onto the wire. Every message becomes one whole line on a
/// channel, and a single thread drains the channel into the writer, so a
/// job's notification and a response cannot interleave. Job threads hold a
/// clone, which is also why the drain outlives EOF until they finish.
#[derive(Debug, Clone)]
pub struct Notifier {
    lines: Sender<String>,
    next_job: Arc<AtomicU64>,
}

impl Notifier {
    /// A notifier and the lines it produces, one message each, newline not
    /// included.
    #[must_use]
    pub fn channel() -> (Self, Receiver<String>) {
        let (lines, receiver) = mpsc::channel();
        let notifier = Self {
            lines,
            next_job: Arc::new(AtomicU64::new(1)),
        };
        (notifier, receiver)
    }

    /// Queues a response; `false` when the writer is gone.
    pub(crate) fn respond(&self, response: &Response) -> bool {
        // A response is built from the wire types; it always encodes.
        serde_json::to_string(response).is_ok_and(|line| self.lines.send(line).is_ok())
    }

    /// Queues a notification. A writer that has gone away ends the session
    /// through the serve loop; a job has nothing to do about it.
    pub fn notify(&self, notification: &Notification) {
        if let Ok(line) = serde_json::to_string(notification) {
            let _ = self.lines.send(line);
        }
    }

    /// Runs `work` on its own thread and answers at once. The job ends with
    /// exactly one `job.done` or `job.failed`, also when `work` panics.
    ///
    /// # Errors
    ///
    /// `Internal` when the thread cannot start.
    pub fn start_job(
        &self,
        work: impl FnOnce(&JobReporter) -> Result<(), JobError> + Send + 'static,
    ) -> Result<JobStarted, RpcError> {
        let job_id = JobId(self.next_job.fetch_add(1, Ordering::Relaxed));
        let reporter = JobReporter {
            job_id,
            notifier: self.clone(),
        };
        thread::Builder::new()
            .name(format!("job-{}", job_id.0))
            .spawn(move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(|| work(&reporter)));
                reporter.finish(outcome.unwrap_or_else(|_| {
                    Err(JobError {
                        code: ErrorCode::InternalError,
                        message: "the job panicked".to_owned(),
                    })
                }));
            })
            .map_err(|error| {
                RpcError::new(
                    ErrorCode::Internal,
                    format!("cannot start a job thread: {error}"),
                )
            })?;
        Ok(JobStarted { job_id })
    }
}

/// Why a job failed, as its `job.failed` carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub code: ErrorCode,
    pub message: String,
}

impl From<CoreError> for JobError {
    fn from(error: CoreError) -> Self {
        let code = match error.code() {
            ErrorCode::Internal => ErrorCode::JobFailed,
            other => other,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// A running job's view of the wire.
#[derive(Debug)]
pub struct JobReporter {
    job_id: JobId,
    notifier: Notifier,
}

impl JobReporter {
    /// The job's id.
    #[must_use]
    pub const fn job_id(&self) -> JobId {
        self.job_id
    }

    /// Emits one `job.progress`.
    pub fn progress(
        &self,
        phase: &str,
        done_bytes: Option<u64>,
        total_bytes: Option<u64>,
        message: impl Into<String>,
    ) {
        self.notifier.notify(&Notification::from(&JobProgress {
            job_id: self.job_id,
            phase: phase.to_owned(),
            done_bytes,
            total_bytes,
            message: message.into(),
        }));
    }

    fn finish(&self, outcome: Result<(), JobError>) {
        let notification = match outcome {
            Ok(()) => Notification::from(&JobDone {
                job_id: self.job_id,
            }),
            Err(error) => Notification::from(&JobFailed {
                job_id: self.job_id,
                code: error.code,
                message: error.message,
            }),
        };
        self.notifier.notify(&notification);
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
