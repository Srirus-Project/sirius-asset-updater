//! Authenticated, bounded job service around the Sirius pipeline.
use crate::{
    jobs::{Job, JobError, JobStore, Limits, Operation, Progress, Request, Status},
    region::Region,
    Error,
};
use axum::{
    extract::{DefaultBodyLimit, Path as HttpPath, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{watch, Mutex, Notify};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[serde(default)]
    pub logging: Option<crate::application_log::Config>,
    pub listen: SocketAddr,
    #[serde(default)]
    pub tls: Option<crate::server::TlsConfig>,
    #[serde(default)]
    pub access_log: Option<crate::access_log::Config>,
    pub token_env: String,
    pub state_directory: PathBuf,
    pub output_directory: PathBuf,
    #[serde(default = "workers")]
    pub max_concurrent_jobs: usize,
    #[serde(default = "queued")]
    pub max_queued_jobs: usize,
    #[serde(default = "retained")]
    pub retain_terminal_jobs: usize,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    pub profiles: BTreeMap<String, Profile>,
}
fn workers() -> usize {
    4
}
fn queued() -> usize {
    64
}
fn retained() -> usize {
    256
}
fn timeout() -> u64 {
    3600
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub region: Region,
    pub download_config: Option<PathBuf>,
    pub export_config: Option<PathBuf>,
    pub storage_config: Option<PathBuf>,
    /// Configured source for standalone verify/export. Update uses its own publication.
    pub input: Option<PathBuf>,
}
impl Profile {
    fn supports(&self, op: Operation) -> bool {
        match op {
            Operation::Update => self.download_config.is_some(),
            Operation::Export => self.input.is_some() && self.export_config.is_some(),
            Operation::Verify => self.input.is_some(),
        }
    }
}
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}
struct Inner {
    access_log: Option<crate::access_log::AccessLog>,
    config: ServiceConfig,
    token: String,
    store: Mutex<JobStore>,
    wake: Notify,
    accepting: AtomicBool,
}
impl Service {
    pub fn open(config: ServiceConfig) -> Result<Self, Error> {
        if let Some(log) = &config.logging {
            log.validate().map_err(|_| Error::Config)?;
        }
        if let Some(tls) = &config.tls {
            tls.validate().map_err(|_| Error::Config)?;
        }
        if config.profiles.is_empty()
            || config.max_concurrent_jobs == 0
            || config.max_concurrent_jobs > 64
            || config.max_queued_jobs == 0
            || config.timeout_seconds == 0
        {
            return Err(Error::Config);
        }
        for (name, p) in &config.profiles {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || p.region == Region::Cn
            {
                return Err(Error::Config);
            }
            if let Some(path) = &p.download_config {
                let c: crate::Config = read_yaml(path)?;
                if c.region != p.region || c.logging.is_some() {
                    return Err(Error::Config);
                }
                c.validate()?;
            }
            if let Some(path) = &p.export_config {
                let export: crate::export::ExportConfig = read_yaml(path)?;
                export.validate()?;
                if export.logging.is_some() {
                    return Err(Error::Config);
                }
                if p.storage_config.is_some() && !export.retain_outputs {
                    return Err(Error::Config);
                }
            }
            if let Some(path) = &p.storage_config {
                if p.export_config.is_none() {
                    return Err(Error::Config);
                }
                let storage: crate::storage::Config = read_yaml(path)?;
                storage.validate()?;
            }
        }
        let token = std::env::var(&config.token_env).map_err(|_| Error::Secret)?;
        if token.trim().is_empty() || token.contains(['\r', '\n']) {
            return Err(Error::Config);
        }
        let access_log = config
            .access_log
            .clone()
            .map(crate::access_log::AccessLog::new)
            .transpose()
            .map_err(|_| Error::Config)?;
        std::fs::create_dir_all(&config.output_directory).map_err(|_| Error::Io)?;
        let store = JobStore::open(
            &config.state_directory,
            Limits {
                max_running: config.max_concurrent_jobs,
                max_queued: config.max_queued_jobs,
                retain_terminal: config.retain_terminal_jobs,
            },
        )
        .map_err(|_| Error::Io)?;
        Ok(Self {
            inner: Arc::new(Inner {
                access_log,
                config,
                token,
                store: Mutex::new(store),
                wake: Notify::new(),
                accepting: AtomicBool::new(true),
            }),
        })
    }
    pub fn router(&self) -> Router {
        let protected = Router::new()
            .route("/api/v1/jobs", get(list).post(submit))
            .route("/api/v1/jobs/{id}", get(detail))
            .route("/api/v1/jobs/{id}/cancel", post(cancel))
            .route("/api/v1/jobs/{id}/retry", post(retry))
            .route_layer(middleware::from_fn_with_state(self.clone(), authorize));
        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            .merge(protected)
            .layer(DefaultBodyLimit::max(8192))
            .with_state(self.clone());
        match &self.inner.access_log {
            Some(log) => log.wrap(router),
            None => router,
        }
    }
    pub async fn run_workers(&self, mut shutdown: watch::Receiver<bool>) -> Result<(), Error> {
        let mut tasks = tokio::task::JoinSet::new();
        let mut controls: HashMap<String, watch::Sender<bool>> = HashMap::new();
        let mut stopping = false;
        let mut storage_error = false;
        loop {
            if *shutdown.borrow() {
                stopping = true;
                self.inner.accepting.store(false, Ordering::Release);
            }
            if stopping {
                for control in controls.values() {
                    let _ = control.send(true);
                }
            } else {
                let mut store = self.inner.store.lock().await;
                for (id, control) in &controls {
                    if store
                        .get(id)
                        .is_some_and(|j| j.status == Status::Cancelling)
                    {
                        let _ = control.send(true);
                    }
                }
                loop {
                    match store.claim() {
                        Ok(Some(job)) => {
                            tracing::info!(job_id=%job.id, region=job.request.region.name(), stage="started", "Job started");
                            let (tx, rx) = watch::channel(false);
                            controls.insert(job.id.clone(), tx.clone());
                            let service = self.clone();
                            tasks.spawn(async move {
                                let result =
                                    std::panic::AssertUnwindSafe(service.execute(&job, tx, rx))
                                        .catch_unwind()
                                        .await;
                                let code = match result {
                                    Ok(Ok(())) => None,
                                    Ok(Err(Error::JobTimeout)) => Some("job_timeout"),
                                    Ok(Err(Error::Cancelled)) => Some("cancelled"),
                                    Ok(Err(_)) => Some("pipeline_failed"),
                                    Err(_) => Some("worker_panicked"),
                                };
                                (job.id, code)
                            });
                        }
                        Ok(None) => break,
                        Err(_) => {
                            storage_error = true;
                            stopping = true;
                            self.inner.accepting.store(false, Ordering::Release);
                            break;
                        }
                    }
                }
            }
            if stopping && tasks.is_empty() {
                break;
            }
            tokio::select! {
                value=tasks.join_next(),if !tasks.is_empty()=>{
                    if let Some(Ok((id,code)))=value {
                        controls.remove(&id);
                        match self.inner.store.lock().await.finish(&id,code) {
                            Ok(job) => {
                                if job.status == Status::Failed { tracing::warn!(job_id=%job.id, region=job.request.region.name(), status=?job.status, error_code=job.failure.as_deref(), "Job ended"); }
                                else { tracing::info!(job_id=%job.id, region=job.request.region.name(), status=?job.status, "Job ended"); }
                            }
                            Err(_) => { tracing::error!(job_id=%id, error_code="job_ledger_failed", "Failed to persist job completion"); storage_error=true;stopping=true; }
                        }
                    } else {storage_error=true;stopping=true;}
                }
                _=self.inner.wake.notified()=>{},
                _=tokio::time::sleep(Duration::from_millis(250))=>{},
                _=shutdown.changed(),if !stopping=>{stopping=true;self.inner.accepting.store(false,Ordering::Release);},
            }
        }
        self.inner.accepting.store(false, Ordering::Release);
        if storage_error {
            Err(Error::Io)
        } else {
            Ok(())
        }
    }
    async fn execute(
        &self,
        job: &Job,
        stop: watch::Sender<bool>,
        receiver: watch::Receiver<bool>,
    ) -> Result<(), Error> {
        let work = self.pipeline(job, stop.clone(), receiver);
        tokio::pin!(work);
        tokio::select! {
            result=&mut work=>result,
            _=tokio::time::sleep(Duration::from_secs(self.inner.config.timeout_seconds))=>{
                let _=stop.send(true);let _=work.await;Err(Error::JobTimeout)
            }
        }
    }
    async fn phase(&self, id: &str, phase: &str) -> Result<(), Error> {
        tracing::info!(job_id = id, stage = phase, "Job stage");
        self.inner
            .store
            .lock()
            .await
            .progress(
                id,
                Progress {
                    phase: phase.into(),
                    ..Progress::default()
                },
            )
            .map_err(|_| Error::Io)?;
        Ok(())
    }
    async fn pipeline(
        &self,
        job: &Job,
        cancel: watch::Sender<bool>,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), Error> {
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
        let profile = self
            .inner
            .config
            .profiles
            .get(&job.request.profile)
            .ok_or(Error::Config)?;
        if profile.region != job.request.region || !profile.supports(job.request.operation) {
            return Err(Error::Config);
        }
        let root = self
            .inner
            .config
            .output_directory
            .join(job.request.region.name())
            .join(&job.id);
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|_| Error::Io)?;
        let input = if job.request.operation == Operation::Update {
            self.phase(&job.id, "download").await?;
            let mut config: crate::Config =
                read_yaml(profile.download_config.as_ref().ok_or(Error::Config)?)?;
            if config.region != profile.region || config.logging.is_some() {
                return Err(Error::Config);
            }
            config.output = root.join("downloads");
            let client = crate::CatalogClient::new(config)?;
            tokio::select! {result=client.fetch()=>result?,_=cancelled(&mut stop)=>return Err(Error::Cancelled)}
        } else {
            profile.input.clone().ok_or(Error::Config)?
        };
        self.phase(&job.id, "verify").await?;
        let verified = tokio::select! {result=crate::verify::verify(&input)=>result?,_=cancelled(&mut stop)=>return Err(Error::Cancelled)};
        if verified.region != profile.region {
            return Err(Error::Snapshot);
        }
        tokio::fs::write(
            root.join("verification.json"),
            sonic_rs::to_vec_pretty(&verified).map_err(|_| Error::Io)?,
        )
        .await
        .map_err(|_| Error::Io)?;
        let mut final_progress = Progress {
            phase: "verify".into(),
            completed: verified.asset_files_verified as u64 + 1,
            failed: 0,
            total: Some(verified.asset_files_verified as u64 + 1),
            bytes: verified.asset_bytes_verified,
        };
        if job.request.operation != Operation::Verify {
            if let Some(path) = &profile.export_config {
                self.phase(&job.id, "export").await?;
                let mut export: crate::export::ExportConfig = read_yaml(path)?;
                if export.logging.is_some()
                    || (profile.storage_config.is_some() && !export.retain_outputs)
                {
                    return Err(Error::Config);
                }
                export.input = input;
                export.output = root.join("exports");
                let output = export.output.clone();
                let task = export.run_controlled(stop.clone());
                tokio::pin!(task);
                let summary = loop {
                    tokio::select! {
                        result=&mut task=>break result?,
                        _=tokio::time::sleep(Duration::from_secs(1))=>{
                            if let Ok(bytes)=tokio::fs::read(output.join("summary.json")).await {
                                if let Ok(s)=sonic_rs::from_slice::<crate::export::ExportSummary>(&bytes){
                                    let progress=Progress {phase:"export".into(),completed:s.succeeded as u64,failed:s.failed as u64,total:Some(s.input_files as u64),bytes:s.output_bytes};
                                    if self.inner.store.lock().await.progress(&job.id,progress).is_err(){
                                        // Signal cancellation and await the exporter; dropping it would detach blocking workers.
                                        let _=cancel.send(true);
                                        let _=task.await;return Err(Error::Io);
                                    }
                                }
                            }
                        }
                    }
                };
                final_progress = Progress {
                    phase: "export".into(),
                    completed: summary.succeeded as u64,
                    failed: summary.failed as u64,
                    total: Some(summary.input_files as u64),
                    bytes: summary.output_bytes,
                };
                if !summary.complete || summary.failed > 0 {
                    return Err(Error::Verification);
                }
                if summary.retained {
                    self.phase(&job.id, "verify_export").await?;
                    let report = tokio::select! {
                        result = crate::export_verify::verify(&output, profile.region) => result?,
                        _ = cancelled(&mut stop) => return Err(Error::Cancelled),
                    };
                    tokio::fs::write(
                        root.join("export-verification.json"),
                        sonic_rs::to_vec_pretty(&report).map_err(|_| Error::Io)?,
                    )
                    .await
                    .map_err(|_| Error::Io)?;
                    final_progress = Progress {
                        phase: "verify_export".into(),
                        completed: report.files_verified as u64,
                        failed: 0,
                        total: Some(report.files_verified as u64),
                        bytes: report.bytes_verified,
                    };
                    if let Some(path) = &profile.storage_config {
                        self.phase(&job.id, "publish").await?;
                        let storage: crate::storage::Config = read_yaml(path)?;
                        let publication = storage
                            .publish(&output, profile.region, stop.clone())
                            .await?;
                        tokio::fs::write(
                            root.join("publication.json"),
                            sonic_rs::to_vec_pretty(&publication).map_err(|_| Error::Io)?,
                        )
                        .await
                        .map_err(|_| Error::Io)?;
                        final_progress = Progress {
                            phase: "publish".into(),
                            completed: publication.files as u64,
                            failed: 0,
                            total: Some(publication.files as u64),
                            bytes: publication.bytes,
                        };
                    }
                }
            }
        }
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
        self.inner
            .store
            .lock()
            .await
            .progress(&job.id, final_progress)
            .map_err(|_| Error::Io)?;
        Ok(())
    }
}
fn read_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    yaml_serde::from_str(&std::fs::read_to_string(path).map_err(|_| Error::Config)?)
        .map_err(|_| Error::Config)
}
pub(crate) async fn cancelled(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}
fn json(status: StatusCode, value: &impl Serialize) -> Response {
    match sonic_rs::to_string(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
fn failure(e: JobError) -> Response {
    let status = match e {
        JobError::Invalid => StatusCode::BAD_REQUEST,
        JobError::Full => StatusCode::TOO_MANY_REQUESTS,
        JobError::NotFound => StatusCode::NOT_FOUND,
        JobError::Conflict => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    json(status, &BTreeMap::from([("error", e.to_string())]))
}
async fn authorize(
    State(service): State<Service>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let expected = format!("Bearer {}", service.inner.token);
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        != Some(expected.as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}
async fn list(State(service): State<Service>) -> Response {
    json(StatusCode::OK, &service.inner.store.lock().await.list())
}
async fn detail(State(service): State<Service>, HttpPath(id): HttpPath<String>) -> Response {
    match service.inner.store.lock().await.get(&id) {
        Some(job) => json(StatusCode::OK, job),
        None => failure(JobError::NotFound),
    }
}
async fn submit(State(service): State<Service>, body: axum::body::Bytes) -> Response {
    if !service.inner.accepting.load(Ordering::Acquire) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(request) = sonic_rs::from_slice::<Request>(&body) else {
        return failure(JobError::Invalid);
    };
    if !service
        .inner
        .config
        .profiles
        .get(&request.profile)
        .is_some_and(|p| p.region == request.region && p.supports(request.operation))
    {
        return failure(JobError::Invalid);
    }
    let result = service.inner.store.lock().await.submit(request);
    match result {
        Ok(job) => {
            service.inner.wake.notify_one();
            json(StatusCode::ACCEPTED, &job)
        }
        Err(e) => failure(e),
    }
}
async fn cancel(State(service): State<Service>, HttpPath(id): HttpPath<String>) -> Response {
    match service.inner.store.lock().await.cancel(&id) {
        Ok(job) => {
            service.inner.wake.notify_one();
            json(StatusCode::ACCEPTED, &job)
        }
        Err(e) => failure(e),
    }
}
async fn retry(State(service): State<Service>, HttpPath(id): HttpPath<String>) -> Response {
    if !service.inner.accepting.load(Ordering::Acquire) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let mut store = service.inner.store.lock().await;
    if let Some(job) = store.get(&id) {
        if !service
            .inner
            .config
            .profiles
            .get(&job.request.profile)
            .is_some_and(|p| p.region == job.request.region && p.supports(job.request.operation))
        {
            return failure(JobError::Invalid);
        }
    }
    match store.retry(&id) {
        Ok(job) => {
            service.inner.wake.notify_one();
            json(StatusCode::ACCEPTED, &job)
        }
        Err(e) => failure(e),
    }
}
pub async fn run_file(path: &Path) -> Result<(), Error> {
    let config: ServiceConfig = read_yaml(path)?;
    let listen = config.listen;
    let tls = config
        .tls
        .as_ref()
        .map(crate::server::TlsConfig::load)
        .transpose()
        .map_err(|_| Error::Config)?;
    let service = Service::open(config)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|_| Error::Io)?;
    tracing::info!(%listen, "Sirius asset service listening");
    let (tx, rx) = watch::channel(false);
    let worker = service.clone();
    let mut workers = tokio::spawn(async move { worker.run_workers(rx).await });
    let mut http_stop = tx.subscribe();
    let http = crate::server::serve(listener, service.router(), tls, async move {
        cancelled(&mut http_stop).await;
    });
    tokio::pin!(http);
    tokio::select! {
        result=&mut http=>{let _=tx.send(true);workers.await.map_err(|_|Error::Io)??;result.map_err(|_|Error::Io)},
        result=&mut workers=>{let _=tx.send(true);http.await.map_err(|_|Error::Io)?;result.map_err(|_|Error::Io)?},
        _=shutdown_signal()=>{service.inner.accepting.store(false,Ordering::Release);let _=tx.send(true);http.await.map_err(|_|Error::Io)?;workers.await.map_err(|_|Error::Io)?}
    }
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Fixture {
        _root: tempfile::TempDir,
        config: ServiceConfig,
        hits: Arc<AtomicUsize>,
        gate: Arc<tokio::sync::Semaphore>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.gate.add_permits(100);
            self.server.abort();
        }
    }
    async fn fixture(timeout_seconds: u64) -> (Fixture, Service) {
        let root = tempfile::tempdir().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let hit = hits.clone();
        let blocked = gate.clone();
        let router = Router::new().route(
            "/internal/v1/resources/snapshot",
            get(move || {
                let hit = hit.clone();
                let blocked = blocked.clone();
                async move {
                    hit.fetch_add(1, Ordering::SeqCst);
                    blocked.acquire().await.unwrap().forget();
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let env = format!("SIRIUS_SERVICE_TEST_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&env, "synthetic-service-token");
        let download = root.path().join("download.yaml");
        let contents = sonic_rs::json!({
            "region":"jp", "game_api_root":url, "internal_token_env":env,
            "environment":"release", "client_version":"1.0.3", "output":"unused",
            "cdn_roots":{"https://static.example":{"username_env":env,"credential_env":env}}
        });
        std::fs::write(&download, sonic_rs::to_vec(&contents).unwrap()).unwrap();
        let config = ServiceConfig {
            logging: None,
            tls: None,
            access_log: None,
            listen: "127.0.0.1:0".parse().unwrap(),
            token_env: env,
            state_directory: root.path().join("ledger"),
            output_directory: root.path().join("outputs"),
            max_concurrent_jobs: 2,
            max_queued_jobs: 8,
            retain_terminal_jobs: 20,
            timeout_seconds,
            profiles: BTreeMap::from([(
                "download".into(),
                Profile {
                    storage_config: None,
                    region: Region::Jp,
                    download_config: Some(download),
                    export_config: None,
                    input: None,
                },
            )]),
        };
        let service = Service::open(config.clone()).unwrap();
        (
            Fixture {
                _root: root,
                config,
                hits,
                gate,
                server,
            },
            service,
        )
    }
    async fn submit(service: &Service) -> Job {
        let job = service
            .inner
            .store
            .lock()
            .await
            .submit(Request {
                region: Region::Jp,
                profile: "download".into(),
                operation: Operation::Update,
            })
            .unwrap();
        service.inner.wake.notify_one();
        job
    }
    async fn status(service: &Service, id: &str) -> Job {
        service.inner.store.lock().await.get(id).unwrap().clone()
    }
    async fn terminal(service: &Service, id: &str) -> Job {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let job = status(service, id).await;
                if job.status.terminal() {
                    break job;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn hits(fixture: &Fixture, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.hits.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    fn workers(
        service: &Service,
    ) -> (
        watch::Sender<bool>,
        tokio::task::JoinHandle<Result<(), Error>>,
    ) {
        let (stop, rx) = watch::channel(false);
        let s = service.clone();
        (stop, tokio::spawn(async move { s.run_workers(rx).await }))
    }
    async fn stop(stop: watch::Sender<bool>, worker: tokio::task::JoinHandle<Result<(), Error>>) {
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn running_cancel_releases_region_after_exit_and_shutdown_preserves_queue_for_restart() {
        let (f, service) = fixture(30).await;
        let first = submit(&service).await;
        let second = submit(&service).await;
        let third = submit(&service).await;
        let (shutdown, worker) = workers(&service);
        hits(&f, 1).await;
        assert_eq!(status(&service, &first.id).await.status, Status::Running);
        assert_eq!(status(&service, &second.id).await.status, Status::Queued);
        assert_eq!(f.hits.load(Ordering::SeqCst), 1); // two slots, but one region
        let cancelling = service.inner.store.lock().await.cancel(&first.id).unwrap();
        assert_eq!(cancelling.status, Status::Cancelling);
        service.inner.wake.notify_one();
        assert_eq!(
            terminal(&service, &first.id).await.status,
            Status::Cancelled
        );
        hits(&f, 2).await;
        assert_eq!(status(&service, &second.id).await.status, Status::Running);
        stop(shutdown, worker).await;
        assert!(!service.inner.accepting.load(Ordering::Acquire));
        assert_eq!(status(&service, &second.id).await.status, Status::Failed);
        assert_eq!(status(&service, &third.id).await.status, Status::Queued);
        assert!(Service::open(f.config.clone()).is_err());
        drop(service);
        let restarted = Service::open(f.config.clone()).unwrap();
        assert_eq!(status(&restarted, &third.id).await.status, Status::Queued);
        let (shutdown, worker) = workers(&restarted);
        hits(&f, 3).await;
        restarted
            .inner
            .store
            .lock()
            .await
            .cancel(&third.id)
            .unwrap();
        restarted.inner.wake.notify_one();
        assert_eq!(
            terminal(&restarted, &third.id).await.status,
            Status::Cancelled
        );
        stop(shutdown, worker).await;
        for job in [&first, &second, &third] {
            let output = f
                .config
                .output_directory
                .join("jp")
                .join(&job.id)
                .join("downloads");
            assert_eq!(std::fs::read_dir(output).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn running_download_timeout_is_failed_with_distinct_code_and_can_retry() {
        let (f, service) = fixture(1).await;
        let first = submit(&service).await;
        let (shutdown, worker) = workers(&service);
        hits(&f, 1).await;
        let failed = terminal(&service, &first.id).await;
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.failure.as_deref(), Some("job_timeout"));
        let retry = service.inner.store.lock().await.retry(&first.id).unwrap();
        service.inner.wake.notify_one();
        hits(&f, 2).await;
        assert_eq!(retry.retry_of.as_deref(), Some(first.id.as_str()));
        service.inner.store.lock().await.cancel(&retry.id).unwrap();
        service.inner.wake.notify_one();
        let cancelled = terminal(&service, &retry.id).await;
        assert_eq!(cancelled.status, Status::Cancelled);
        assert!(cancelled.failure.is_none());
        stop(shutdown, worker).await;
    }

    #[tokio::test]
    async fn interrupted_download_is_failed_on_reopen_and_queued_work_is_preserved() {
        let (f, service) = fixture(30).await;
        let first = submit(&service).await;
        let queued = submit(&service).await;
        let (_shutdown, worker) = workers(&service);
        hits(&f, 1).await;
        // Simulate loss of the worker during async download; no blocking export is active.
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        drop(service);
        let restarted = Service::open(f.config.clone()).unwrap();
        let interrupted = status(&restarted, &first.id).await;
        assert_eq!(interrupted.status, Status::Failed);
        assert_eq!(interrupted.failure.as_deref(), Some("service_interrupted"));
        assert_eq!(status(&restarted, &queued.id).await.status, Status::Queued);
    }
}
