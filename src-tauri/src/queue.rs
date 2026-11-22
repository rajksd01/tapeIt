//! The Drive upload queue: one background worker, retries with backoff, survives app restarts.
//!
//!   enqueue ─▶ Waiting ─(due)─▶ Uploading ─ok─▶ (removed, link saved in library.json)
//!                 ▲                 │
//!                 └──backoff────────┤ transient failure (10 s, 60 s, 300 s, then Failed)
//!                                   └ sign-in failure: stays Waiting, paused until reconnect

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::drive::{self, Client, Failure, FailureKind, Flow};
use crate::library;

const BACKOFF_SECONDS: [u64; 3] = [10, 60, 300];
const SIGN_IN_PAUSE_SECONDS: u64 = 30;
const IDLE_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Waiting,
    Uploading,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub path: PathBuf,
    pub state: JobState,
    pub error: Option<String>,
    attempts: usize,
    next_try: u64,
    session: Option<String>,
}

/// Queue state without I/O, so the rules are testable.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Jobs(Vec<Job>);

impl Jobs {
    fn find(&mut self, path: &Path) -> Option<&mut Job> {
        self.0.iter_mut().find(|job| job.path == path)
    }

    /// Adds a job unless the file is already queued. A failed job is retried instead.
    fn enqueue(&mut self, path: &Path, now: u64) {
        if let Some(job) = self.find(path) {
            if job.state == JobState::Failed {
                job.state = JobState::Waiting;
                job.attempts = 0;
                job.next_try = now;
            }
            return;
        }
        self.0.push(Job { path: path.into(), state: JobState::Waiting, error: None, attempts: 0, next_try: now, session: None });
    }

    /// Marks the next due job as uploading and returns it.
    fn take_due(&mut self, now: u64) -> Option<Job> {
        let job = self.0.iter_mut().find(|job| job.state == JobState::Waiting && job.next_try <= now)?;
        job.state = JobState::Uploading;
        Some(job.clone())
    }

    fn fail(&mut self, path: &Path, failure: &Failure, now: u64) {
        let Some(job) = self.find(path) else {
            return;
        };
        job.error = Some(failure.message.clone());
        job.session = failure.session.clone();

        // Sign-in trouble is not the file's fault: wait for reconnect without burning attempts.
        if failure.kind == FailureKind::SignIn {
            job.state = JobState::Waiting;
            job.next_try = now + SIGN_IN_PAUSE_SECONDS;
            return;
        }

        job.attempts += 1;
        let Some(delay) = BACKOFF_SECONDS.get(job.attempts - 1) else {
            job.state = JobState::Failed;
            return;
        };
        job.state = JobState::Waiting;
        job.next_try = now + delay;
    }

    fn remove(&mut self, path: &Path) {
        self.0.retain(|job| job.path != path);
    }

    /// After a crash, an "uploading" job is just waiting; its session lets it resume.
    fn recover(&mut self) {
        for job in self.0.iter_mut().filter(|job| job.state == JobState::Uploading) {
            job.state = JobState::Waiting;
        }
    }
}

/// What the worker tells the UI.
pub enum Notice {
    Progress(PathBuf, f64),
    Changed,
}

/// The queue shared by commands and the worker thread.
pub struct Uploads {
    jobs: Mutex<Jobs>,
    cancelled: Mutex<HashSet<PathBuf>>,
    file: PathBuf,
    library_cache: PathBuf,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs())
}

impl Uploads {
    pub fn load(file: PathBuf, library_cache: PathBuf) -> Arc<Self> {
        let mut jobs: Jobs = fs::read(&file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        jobs.recover();
        Arc::new(Uploads { jobs: Mutex::new(jobs), cancelled: Mutex::new(HashSet::new()), file, library_cache })
    }

    fn save(&self, jobs: &Jobs) {
        if let Some(parent) = self.file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec(jobs) {
            let _ = fs::write(&self.file, bytes);
        }
    }

    /// Runs `change` on the jobs and saves the result.
    fn update<T>(&self, change: impl FnOnce(&mut Jobs) -> T) -> T {
        let mut jobs = self.jobs.lock().unwrap();
        let result = change(&mut jobs);
        self.save(&jobs);
        result
    }

    pub fn enqueue(&self, path: &Path) {
        self.cancelled.lock().unwrap().remove(path);
        self.update(|jobs| jobs.enqueue(path, now()));
    }

    /// Drops a job; a running upload stops after its current chunk.
    pub fn cancel(&self, path: &Path) {
        self.cancelled.lock().unwrap().insert(path.into());
        self.update(|jobs| jobs.remove(path));
    }

    pub fn jobs(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().0.clone()
    }

    /// Claims the next due job. Saves only when one was claimed: the worker polls every second.
    fn take_due(&self) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs.take_due(now())?;
        self.save(&jobs);
        Some(job)
    }

    fn is_cancelled(&self, path: &Path) -> bool {
        self.cancelled.lock().unwrap().contains(path)
    }

    fn run(&self, job: Job, client: &Client, notify: &impl Fn(Notice)) {
        let path = job.path.clone();
        let result = drive::upload(client, &path, job.session, |fraction| {
            notify(Notice::Progress(path.clone(), fraction));
            if self.is_cancelled(&path) {
                return Flow::Stop;
            }
            Flow::Continue
        });

        if self.is_cancelled(&path) {
            return;
        }
        match result {
            Ok(uploaded) => {
                let _ = library::set_drive(&self.library_cache, &path, &uploaded.id, &uploaded.link);
                self.update(|jobs| jobs.remove(&path));
            }
            Err(failure) => self.update(|jobs| jobs.fail(&path, &failure, now())),
        }
    }
}

/// Starts the worker. `client` reads the current OAuth client from settings on every job.
pub fn start(uploads: Arc<Uploads>, client: impl Fn() -> Client + Send + 'static, notify: impl Fn(Notice) + Send + 'static) {
    thread::spawn(move || loop {
        let Some(job) = uploads.take_due() else {
            thread::sleep(IDLE_POLL);
            continue;
        };
        notify(Notice::Changed);
        uploads.run(job, &client(), &notify);
        notify(Notice::Changed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transient() -> Failure {
        Failure { message: "offline".into(), session: Some("session-1".into()), kind: FailureKind::Transient }
    }

    #[test]
    fn enqueue_dedupe() {
        let mut jobs = Jobs::default();
        jobs.enqueue(Path::new("/a.mov"), 0);
        jobs.enqueue(Path::new("/a.mov"), 0);
        assert_eq!(jobs.0.len(), 1);
    }

    #[test]
    fn retry_backs_off_then_fails() {
        let mut jobs = Jobs::default();
        let path = Path::new("/a.mov");
        jobs.enqueue(path, 0);

        for (attempt, delay) in BACKOFF_SECONDS.iter().enumerate() {
            let now = 1000 * attempt as u64;
            let job = jobs.take_due(u64::MAX).unwrap();
            assert_eq!(job.state, JobState::Uploading);
            jobs.fail(path, &transient(), now);
            assert_eq!(jobs.0[0].state, JobState::Waiting);
            assert_eq!(jobs.0[0].next_try, now + delay);
            assert!(jobs.take_due(now).is_none(), "not due before backoff");
        }

        jobs.take_due(u64::MAX).unwrap();
        jobs.fail(path, &transient(), 0);
        assert_eq!(jobs.0[0].state, JobState::Failed);
        assert_eq!(jobs.0[0].session.as_deref(), Some("session-1"));

        // Enqueueing a failed file retries it from scratch.
        jobs.enqueue(path, 5);
        assert_eq!(jobs.0[0].state, JobState::Waiting);
        assert_eq!(jobs.0[0].attempts, 0);
    }

    #[test]
    fn auth_revoked_keeps_job_without_attempts() {
        let mut jobs = Jobs::default();
        let path = Path::new("/a.mov");
        jobs.enqueue(path, 0);
        jobs.take_due(0).unwrap();
        jobs.fail(path, &Failure { message: "Reconnect".into(), session: None, kind: FailureKind::SignIn }, 100);

        assert_eq!(jobs.0[0].state, JobState::Waiting);
        assert_eq!(jobs.0[0].attempts, 0);
        assert_eq!(jobs.0[0].next_try, 100 + SIGN_IN_PAUSE_SECONDS);
    }

    #[test]
    fn queue_persist_and_recover() {
        let folder = std::env::temp_dir().join("screenpen-queue-test");
        let _ = fs::remove_dir_all(&folder);
        let file = folder.join("queue.json");

        let uploads = Uploads::load(file.clone(), folder.join("library.json"));
        uploads.enqueue(Path::new("/a.mov"));
        uploads.update(|jobs| jobs.take_due(u64::MAX)).unwrap();
        assert_eq!(uploads.jobs()[0].state, JobState::Uploading);

        // A restart turns the interrupted upload back into a waiting job.
        let reloaded = Uploads::load(file, folder.join("library.json"));
        assert_eq!(reloaded.jobs()[0].state, JobState::Waiting);
    }

    #[test]
    fn delete_uploading_cancels() {
        let folder = std::env::temp_dir().join("screenpen-queue-cancel");
        let _ = fs::remove_dir_all(&folder);
        let uploads = Uploads::load(folder.join("queue.json"), folder.join("library.json"));
        let path = Path::new("/a.mov");

        uploads.enqueue(path);
        uploads.cancel(path);
        assert!(uploads.jobs().is_empty());
        assert!(uploads.is_cancelled(path));

        uploads.enqueue(path);
        assert!(!uploads.is_cancelled(path));
    }
}
