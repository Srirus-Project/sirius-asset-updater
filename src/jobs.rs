//! Durable, single-owner job ledger. Workers acknowledge cancellation before releasing slots.
use crate::region::Region;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("job state is already owned by another service")]
    Locked,
    #[error("invalid job request or service limits")]
    Invalid,
    #[error("job queue is full")]
    Full,
    #[error("job not found")]
    NotFound,
    #[error("job transition conflicts with its current state")]
    Conflict,
    #[error("job state could not be read or persisted")]
    Storage,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}
impl Status {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    fn occupies_slot(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Update,
    Export,
    Verify,
}
/// Requests select configured profiles; callers cannot supply filesystem paths or credentials.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub region: Region,
    pub profile: String,
    pub operation: Operation,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Progress {
    pub phase: String,
    pub completed: u64,
    pub failed: u64,
    pub total: Option<u64>,
    pub bytes: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub id: String,
    pub request: Request,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub progress: Progress,
    /// Stable sanitized error code, never a raw upstream response or secret-bearing log.
    pub failure: Option<String>,
    pub retry_of: Option<String>,
    /// Digest only: the raw caller key is never stored or returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_sha256: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Ledger {
    schema_version: u8,
    jobs: Vec<Job>,
}
#[derive(Clone, Copy)]
pub struct Limits {
    pub max_running: usize,
    pub max_queued: usize,
    pub retain_terminal: usize,
}
pub struct JobStore {
    directory: PathBuf,
    _owner: File,
    ledger: Ledger,
    limits: Limits,
}
impl JobStore {
    pub fn open(directory: &Path, limits: Limits) -> Result<Self, JobError> {
        if limits.max_running == 0 || limits.max_queued == 0 {
            return Err(JobError::Invalid);
        }
        fs::create_dir_all(directory).map_err(|_| JobError::Storage)?;
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("owner.lock"))
            .map_err(|_| JobError::Storage)?;
        owner.try_lock().map_err(|_| JobError::Locked)?;
        let path = directory.join("jobs.json");
        let mut ledger: Ledger = match fs::read(&path) {
            Ok(bytes) => sonic_rs::from_slice(&bytes).map_err(|_| JobError::Storage)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger {
                schema_version: 1,
                jobs: vec![],
            },
            Err(_) => return Err(JobError::Storage),
        };
        if ledger.schema_version != 1 {
            return Err(JobError::Storage);
        }
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for job in &mut ledger.jobs {
            if let Some(key) = &job.idempotency_sha256 {
                if key.len() != 64
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || !keys.insert(key.clone())
                {
                    return Err(JobError::Storage);
                }
            }
            if uuid::Uuid::parse_str(&job.id).is_err()
                || !ids.insert(job.id.clone())
                || !valid_request(&job.request)
            {
                return Err(JobError::Storage);
            }
            if job.status.occupies_slot() {
                job.status = Status::Failed;
                job.failure = Some("service_interrupted".into());
                job.updated_at = Utc::now();
            }
        }
        let mut store = Self {
            directory: directory.into(),
            _owner: owner,
            ledger,
            limits,
        };
        store.commit(store.ledger.clone())?;
        Ok(store)
    }
    pub fn list(&self) -> &[Job] {
        &self.ledger.jobs
    }
    pub fn get(&self, id: &str) -> Option<&Job> {
        self.ledger.jobs.iter().find(|j| j.id == id)
    }
    pub fn submit(&mut self, request: Request) -> Result<Job, JobError> {
        self.enqueue(request, None, None)
    }
    /// Safe resubmission while the original job remains in the retained ledger.
    pub fn submit_idempotent(&mut self, request: Request, key: &str) -> Result<Job, JobError> {
        if !valid_request(&request)
            || key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(JobError::Invalid);
        }
        let digest = hex::encode(Sha256::digest(key.as_bytes()));
        if let Some(job) = self
            .ledger
            .jobs
            .iter()
            .find(|j| j.idempotency_sha256.as_ref() == Some(&digest))
        {
            return if job.request == request {
                Ok(job.clone())
            } else {
                Err(JobError::Conflict)
            };
        }
        self.enqueue(request, None, Some(digest))
    }
    fn enqueue(
        &mut self,
        request: Request,
        retry_of: Option<String>,
        idempotency_sha256: Option<String>,
    ) -> Result<Job, JobError> {
        if !valid_request(&request) {
            return Err(JobError::Invalid);
        }
        if self
            .ledger
            .jobs
            .iter()
            .filter(|j| j.status == Status::Queued)
            .count()
            >= self.limits.max_queued
        {
            return Err(JobError::Full);
        }
        let now = Utc::now();
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            request,
            status: Status::Queued,
            created_at: now,
            updated_at: now,
            progress: Progress::default(),
            failure: None,
            retry_of,
            idempotency_sha256,
        };
        let mut ledger = self.ledger.clone();
        ledger.jobs.push(job.clone());
        self.commit(ledger)?;
        Ok(job)
    }
    pub fn retry(&mut self, id: &str) -> Result<Job, JobError> {
        let job = self.get(id).ok_or(JobError::NotFound)?;
        if !matches!(job.status, Status::Failed | Status::Cancelled) {
            return Err(JobError::Conflict);
        }
        self.enqueue(job.request.clone(), Some(id.into()), None)
    }
    /// FIFO among runnable regions. A busy region cannot block an idle region's queue.
    pub fn claim(&mut self) -> Result<Option<Job>, JobError> {
        let active: Vec<_> = self
            .ledger
            .jobs
            .iter()
            .filter(|j| j.status.occupies_slot())
            .map(|j| j.request.region)
            .collect();
        if active.len() >= self.limits.max_running {
            return Ok(None);
        }
        let Some(index) = self
            .ledger
            .jobs
            .iter()
            .position(|j| j.status == Status::Queued && !active.contains(&j.request.region))
        else {
            return Ok(None);
        };
        let mut ledger = self.ledger.clone();
        let job = &mut ledger.jobs[index];
        job.status = Status::Running;
        job.updated_at = Utc::now();
        let result = job.clone();
        self.commit(ledger)?;
        Ok(Some(result))
    }
    pub fn cancel(&mut self, id: &str) -> Result<Job, JobError> {
        self.mutate(id, |job| {
            job.status = match job.status {
                Status::Queued => Status::Cancelled,
                Status::Running | Status::Cancelling => Status::Cancelling,
                _ => return Err(JobError::Conflict),
            };
            Ok(())
        })
    }
    pub fn progress(&mut self, id: &str, progress: Progress) -> Result<Job, JobError> {
        if progress.phase.len() > 64
            || !progress
                .phase
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            || progress
                .total
                .is_some_and(|n| progress.completed.saturating_add(progress.failed) > n)
        {
            return Err(JobError::Invalid);
        }
        self.mutate(id, |job| {
            if !job.status.occupies_slot() {
                return Err(JobError::Conflict);
            }
            job.progress = progress;
            Ok(())
        })
    }
    /// Called only after the worker (including media subprocesses) has stopped.
    pub fn finish(&mut self, id: &str, failure_code: Option<&str>) -> Result<Job, JobError> {
        if failure_code.is_some_and(|s| {
            s.is_empty() || s.len() > 64 || !s.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')
        }) {
            return Err(JobError::Invalid);
        }
        self.mutate(id, |job| {
            job.status = match job.status {
                Status::Cancelling => Status::Cancelled,
                Status::Running if failure_code.is_some() || job.progress.failed > 0 => {
                    Status::Failed
                }
                Status::Running => Status::Completed,
                _ => return Err(JobError::Conflict),
            };
            job.failure = if job.status == Status::Failed {
                Some(failure_code.unwrap_or("partial_failure").into())
            } else {
                None
            };
            Ok(())
        })
    }
    fn mutate(
        &mut self,
        id: &str,
        apply: impl FnOnce(&mut Job) -> Result<(), JobError>,
    ) -> Result<Job, JobError> {
        let mut ledger = self.ledger.clone();
        let job = ledger
            .jobs
            .iter_mut()
            .find(|j| j.id == id)
            .ok_or(JobError::NotFound)?;
        apply(job)?;
        job.updated_at = Utc::now();
        let result = job.clone();
        self.commit(ledger)?;
        Ok(result)
    }
    fn commit(&mut self, mut ledger: Ledger) -> Result<(), JobError> {
        if self.limits.retain_terminal > 0 {
            let mut terminal: Vec<_> = ledger
                .jobs
                .iter()
                .filter(|j| j.status.terminal())
                .map(|j| (j.updated_at, j.id.clone()))
                .collect();
            terminal.sort();
            let remove = terminal.len().saturating_sub(self.limits.retain_terminal);
            let ids: std::collections::HashSet<_> = terminal
                .into_iter()
                .take(remove)
                .map(|(_, id)| id)
                .collect();
            ledger.jobs.retain(|j| !ids.contains(&j.id));
        }
        let bytes = sonic_rs::to_vec(&ledger).map_err(|_| JobError::Storage)?;
        let mut file =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| JobError::Storage)?;
        file.write_all(&bytes).map_err(|_| JobError::Storage)?;
        file.as_file().sync_all().map_err(|_| JobError::Storage)?;
        file.persist(self.directory.join("jobs.json"))
            .map_err(|_| JobError::Storage)?;
        self.ledger = ledger;
        Ok(())
    }
}
fn valid_request(request: &Request) -> bool {
    request.region != Region::Cn
        && !request.profile.is_empty()
        && request.profile.len() <= 64
        && request
            .profile
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits() -> Limits {
        Limits {
            max_running: 2,
            max_queued: 8,
            retain_terminal: 2,
        }
    }
    fn request(region: Region) -> Request {
        Request {
            region,
            profile: "full".into(),
            operation: Operation::Update,
        }
    }
    #[test]
    fn idempotent_submission_survives_restart_and_queue_saturation() {
        let d = tempfile::tempdir().unwrap();
        let mut limits = limits();
        limits.max_queued = 1;
        let mut store = JobStore::open(d.path(), limits).unwrap();
        let original = store
            .submit_idempotent(request(Region::Jp), "jp:catalog-1")
            .unwrap();
        assert_eq!(
            store
                .submit_idempotent(request(Region::Jp), "jp:catalog-1")
                .unwrap()
                .id,
            original.id
        );
        assert!(matches!(
            store.submit_idempotent(request(Region::En), "jp:catalog-1"),
            Err(JobError::Conflict)
        ));
        let mut changed = request(Region::Jp);
        changed.operation = Operation::Verify;
        assert!(matches!(
            store.submit_idempotent(changed, "jp:catalog-1"),
            Err(JobError::Conflict)
        ));
        assert!(matches!(
            store.submit_idempotent(request(Region::Jp), "another"),
            Err(JobError::Full)
        ));
        assert!(!fs::read_to_string(d.path().join("jobs.json"))
            .unwrap()
            .contains("jp:catalog-1"));
        store.claim().unwrap();
        drop(store);
        let mut store = JobStore::open(d.path(), limits).unwrap();
        let replay = store
            .submit_idempotent(request(Region::Jp), "jp:catalog-1")
            .unwrap();
        assert_eq!(replay.id, original.id);
        assert_eq!(replay.failure.as_deref(), Some("service_interrupted"));
        let retry = store.retry(&original.id).unwrap();
        assert!(retry.idempotency_sha256.is_none());
        assert_ne!(retry.id, original.id);
        assert_eq!(
            store
                .submit_idempotent(request(Region::Jp), "jp:catalog-1")
                .unwrap()
                .id,
            original.id
        );
    }
    #[test]
    fn idempotency_validation_retention_and_failed_commit() {
        let d = tempfile::tempdir().unwrap();
        let mut limits = limits();
        limits.retain_terminal = 1;
        let mut store = JobStore::open(d.path(), limits).unwrap();
        for key in [
            "",
            "contains space",
            "comma,combined",
            "bad/uri",
            "非ASCII",
            &"a".repeat(129),
        ] {
            assert!(matches!(
                store.submit_idempotent(request(Region::Jp), key),
                Err(JobError::Invalid)
            ));
        }
        let first = store
            .submit_idempotent(request(Region::Jp), "key-1")
            .unwrap();
        store.cancel(&first.id).unwrap();
        let second = store
            .submit_idempotent(request(Region::Jp), "key-2")
            .unwrap();
        store.cancel(&second.id).unwrap();
        assert!(store.get(&first.id).is_none());
        let replacement = store
            .submit_idempotent(request(Region::Jp), "key-1")
            .unwrap();
        assert_ne!(replacement.id, first.id);
        fs::remove_file(d.path().join("jobs.json")).unwrap();
        fs::create_dir(d.path().join("jobs.json")).unwrap();
        assert!(matches!(
            store.submit_idempotent(request(Region::Jp), "not-committed"),
            Err(JobError::Storage)
        ));
        assert_eq!(store.list().len(), 2);
        fs::remove_dir(d.path().join("jobs.json")).unwrap();
        let committed = store
            .submit_idempotent(request(Region::Jp), "not-committed")
            .unwrap();
        drop(store);
        let mut store = JobStore::open(d.path(), limits).unwrap();
        assert_eq!(
            store
                .submit_idempotent(request(Region::Jp), "not-committed")
                .unwrap()
                .id,
            committed.id
        );
        // Duplicate digests or malformed persisted identities must not be accepted.
        store.ledger.jobs[0].idempotency_sha256 = committed.idempotency_sha256;
        fs::write(
            d.path().join("jobs.json"),
            sonic_rs::to_vec(&store.ledger).unwrap(),
        )
        .unwrap();
        drop(store);
        assert!(matches!(
            JobStore::open(d.path(), limits),
            Err(JobError::Storage)
        ));
    }
    #[test]
    fn cancellation_holds_region_and_slot_until_worker_exit() {
        let d = tempfile::tempdir().unwrap();
        let mut s = JobStore::open(d.path(), limits()).unwrap();
        let first = s.submit(request(Region::Jp)).unwrap();
        s.submit(request(Region::Jp)).unwrap();
        let en = s.submit(request(Region::En)).unwrap();
        assert_eq!(s.claim().unwrap().unwrap().id, first.id);
        assert_eq!(s.cancel(&first.id).unwrap().status, Status::Cancelling);
        assert_eq!(s.claim().unwrap().unwrap().id, en.id);
        assert!(s.claim().unwrap().is_none());
        assert_eq!(s.finish(&first.id, None).unwrap().status, Status::Cancelled);
        assert_eq!(s.claim().unwrap().unwrap().request.region, Region::Jp);
        assert!(matches!(s.finish(&first.id, None), Err(JobError::Conflict)));
    }
    #[test]
    fn restart_marks_inflight_failed_but_preserves_queue_and_enforces_single_owner() {
        let d = tempfile::tempdir().unwrap();
        let mut s = JobStore::open(d.path(), limits()).unwrap();
        assert!(matches!(
            JobStore::open(d.path(), limits()),
            Err(JobError::Locked)
        ));
        let active = s.submit(request(Region::Jp)).unwrap();
        let queued = s.submit(request(Region::En)).unwrap();
        s.claim().unwrap();
        drop(s);
        let mut s = JobStore::open(d.path(), limits()).unwrap();
        assert_eq!(
            s.get(&active.id).unwrap().failure.as_deref(),
            Some("service_interrupted")
        );
        assert_eq!(s.claim().unwrap().unwrap().id, queued.id);
        let retry = s.retry(&active.id).unwrap();
        assert_eq!(retry.retry_of.as_deref(), Some(active.id.as_str()));
    }
    #[test]
    fn limits_and_retention_do_not_remove_active_work() {
        let d = tempfile::tempdir().unwrap();
        let mut l = limits();
        l.max_queued = 1;
        let mut s = JobStore::open(d.path(), l).unwrap();
        let active = s.submit(request(Region::En)).unwrap();
        assert!(matches!(s.submit(request(Region::Jp)), Err(JobError::Full)));
        s.claim().unwrap();
        for _ in 0..4 {
            let j = s.submit(request(Region::Jp)).unwrap();
            s.cancel(&j.id).unwrap();
        }
        assert_eq!(s.list().len(), 3);
        assert_eq!(s.get(&active.id).unwrap().status, Status::Running);
    }
    #[test]
    fn failed_persistence_does_not_claim_or_acknowledge_new_work() {
        let d = tempfile::tempdir().unwrap();
        let mut s = JobStore::open(d.path(), limits()).unwrap();
        s.submit(request(Region::Jp)).unwrap();
        fs::remove_file(d.path().join("jobs.json")).unwrap();
        fs::create_dir(d.path().join("jobs.json")).unwrap();
        assert!(matches!(s.claim(), Err(JobError::Storage)));
        assert_eq!(s.list()[0].status, Status::Queued);
    }
    #[test]
    fn rejects_reserved_regions_paths_and_corrupt_state() {
        let d = tempfile::tempdir().unwrap();
        let mut s = JobStore::open(d.path(), limits()).unwrap();
        assert!(matches!(
            s.submit(request(Region::Cn)),
            Err(JobError::Invalid)
        ));
        let mut r = request(Region::Jp);
        r.profile = "../secrets".into();
        assert!(matches!(s.submit(r), Err(JobError::Invalid)));
        drop(s);
        fs::write(d.path().join("jobs.json"), b"invalid").unwrap();
        assert!(matches!(
            JobStore::open(d.path(), limits()),
            Err(JobError::Storage)
        ));
    }
}
