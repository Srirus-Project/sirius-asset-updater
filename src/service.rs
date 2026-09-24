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
    pub listen: SocketAddr,
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
    config: ServiceConfig,
    token: String,
    store: Mutex<JobStore>,
    wake: Notify,
    accepting: AtomicBool,
}
impl Service {
    pub fn open(config: ServiceConfig) -> Result<Self, Error> {
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
                if c.region != p.region {
                    return Err(Error::Config);
                }
                c.validate()?;
            }
            if let Some(path) = &p.export_config {
                let _: crate::export::ExportConfig = read_yaml(path)?;
            }
        }
        let token = std::env::var(&config.token_env).map_err(|_| Error::Secret)?;
        if token.trim().is_empty() || token.contains(['\r', '\n']) {
            return Err(Error::Config);
        }
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
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .merge(protected)
            .layer(DefaultBodyLimit::max(8192))
            .with_state(self.clone())
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
                        if self.inner.store.lock().await.finish(&id,code).is_err(){storage_error=true;stopping=true;}
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
        let work = self.pipeline(job, receiver);
        tokio::pin!(work);
        tokio::select! {
            result=&mut work=>result,
            _=tokio::time::sleep(Duration::from_secs(self.inner.config.timeout_seconds))=>{
                let _=stop.send(true);let _=work.await;Err(Error::Cancelled)
            }
        }
    }
    async fn phase(&self, id: &str, phase: &str) -> Result<(), Error> {
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
    async fn pipeline(&self, job: &Job, mut stop: watch::Receiver<bool>) -> Result<(), Error> {
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
            if config.region != profile.region {
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
        if job.request.operation != Operation::Verify {
            if let Some(path) = &profile.export_config {
                self.phase(&job.id, "export").await?;
                let mut export: crate::export::ExportConfig = read_yaml(path)?;
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
                                        // Keep waiting for the exporter; dropping it would detach blocking workers.
                                        let _=task.await;return Err(Error::Io);
                                    }
                                }
                            }
                        }
                    }
                };
                if !summary.complete || summary.failed > 0 {
                    return Err(Error::Verification);
                }
            }
        }
        if *stop.borrow() {
            return Err(Error::Cancelled);
        }
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
    let service = Service::open(config)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|_| Error::Io)?;
    let (tx, rx) = watch::channel(false);
    let worker = service.clone();
    let mut workers = tokio::spawn(async move { worker.run_workers(rx).await });
    let mut http_stop = tx.subscribe();
    let http = axum::serve(listener, service.router()).with_graceful_shutdown(async move {
        cancelled(&mut http_stop).await;
    });
    let http = std::future::IntoFuture::into_future(http);
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
