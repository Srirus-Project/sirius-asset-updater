//! Bounded access logging and explicit trusted-hop resolution. Never logs raw URLs.
use axum::{
    extract::{ConnectInfo, MatchedPath, Request, State},
    http::{HeaderMap, HeaderName},
    middleware::{self, Next},
    response::Response,
    Router,
};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    Json,
    Text,
    /// Access logs only: renders `template` placeholders; see [`Placeholder`].
    Template,
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    Never,
    Hourly,
    #[default]
    Daily,
}
#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Output {
    Stdout {},
    Stderr {},
    File {
        path: PathBuf,
        #[serde(default)]
        rotation: Rotation,
        #[serde(default = "max_files")]
        max_files: usize,
    },
}
impl Default for Output {
    fn default() -> Self {
        Self::Stdout {}
    }
}
fn max_files() -> usize {
    7
}
fn queue_capacity() -> usize {
    4096
}
fn proxy_header() -> String {
    "x-forwarded-for".into()
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub format: Format,
    pub output: Output,
    pub queue_capacity: usize,
    pub trusted_proxies: Vec<String>,
    pub proxy_header: String,
    /// Required with, and only accepted with, `format: template`.
    pub template: Option<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            format: Format::default(),
            output: Output::default(),
            queue_capacity: queue_capacity(),
            trusted_proxies: vec![],
            proxy_header: proxy_header(),
            template: None,
        }
    }
}
impl Config {
    pub fn validate(&self) -> io::Result<()> {
        self.trust()?;
        self.template()?;
        if !(1..=65536).contains(&self.queue_capacity) {
            return Err(invalid());
        }
        if let Output::File {
            path, max_files, ..
        } = &self.output
        {
            if path.file_name().and_then(|p| p.to_str()).is_none()
                || !(1..=3650).contains(max_files)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
    fn trust(&self) -> io::Result<Trust> {
        if self.trusted_proxies.len() > 128 {
            return Err(invalid());
        }
        let header = HeaderName::from_bytes(self.proxy_header.as_bytes()).map_err(|_| invalid())?;
        // These cannot be reinterpreted as source-address assertions.
        if matches!(
            header.as_str(),
            "authorization" | "proxy-authorization" | "cookie" | "host"
        ) {
            return Err(invalid());
        }
        let proxies = self
            .trusted_proxies
            .iter()
            .map(|p| p.parse().map_err(|_| invalid()))
            .collect::<io::Result<Vec<IpNet>>>()?;
        Ok(Trust { proxies, header })
    }
    fn template(&self) -> io::Result<Option<Vec<Part>>> {
        match (&self.format, &self.template) {
            (Format::Template, Some(template)) => parse_template(template).map(Some),
            (Format::Template, None) | (_, Some(_)) => Err(invalid()),
            _ => Ok(None),
        }
    }
}
/// Original Haruki placeholders plus Sirius record fields. `path` is the matched route
/// template, never the raw request path or query.
#[derive(Clone, Copy)]
enum Placeholder {
    Time,
    Status,
    Method,
    Path,
    Latency,
    RequestId,
    PeerIp,
    ClientIp,
    Outcome,
    DurationMs,
    QueueDroppedRecords,
}
impl Placeholder {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "time" => Self::Time,
            "status" => Self::Status,
            "method" => Self::Method,
            "path" | "route" => Self::Path,
            "latency" => Self::Latency,
            "request_id" => Self::RequestId,
            "peer_ip" => Self::PeerIp,
            "client_ip" => Self::ClientIp,
            "outcome" => Self::Outcome,
            "duration_ms" => Self::DurationMs,
            "queue_dropped_records" => Self::QueueDroppedRecords,
            _ => return None,
        })
    }
    fn value(self, record: &Record, elapsed: Duration) -> String {
        let ip = |ip: Option<IpAddr>| ip.map_or_else(|| "-".into(), |p| p.to_string());
        match self {
            Self::Time => record.timestamp.clone(),
            Self::Status => record.status.map_or_else(|| "-".into(), |s| s.to_string()),
            Self::Method => record.method.clone(),
            Self::Path => record.route.clone(),
            // Same rendering as the original: two decimals, seconds from one second upward.
            Self::Latency => {
                let ms = elapsed.as_secs_f64() * 1000.0;
                if ms >= 1000.0 {
                    format!("{:.2}s", ms / 1000.0)
                } else {
                    format!("{ms:.2}ms")
                }
            }
            Self::RequestId => record.request_id.clone(),
            Self::PeerIp => ip(record.peer_ip),
            Self::ClientIp => ip(record.client_ip),
            Self::Outcome => record.outcome.into(),
            Self::DurationMs => record.duration_ms.to_string(),
            Self::QueueDroppedRecords => record.queue_dropped_records.to_string(),
        }
    }
}
#[derive(Clone)]
enum Part {
    Literal(String),
    Value(Placeholder),
}
fn line_breaking(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}
fn parse_template(template: &str) -> io::Result<Vec<Part>> {
    // The original appended a newline unless the template already ended with one.
    let template = template.strip_suffix('\n').unwrap_or(template);
    if template.is_empty() || template.len() > 1024 || template.chars().any(line_breaking) {
        return Err(invalid());
    }
    let mut parts = vec![];
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            parts.push(Part::Literal(rest[..start].into()));
        }
        let (name, after) = rest[start + 2..].split_once('}').ok_or_else(invalid)?;
        parts.push(Part::Value(Placeholder::parse(name).ok_or_else(invalid)?));
        rest = after;
    }
    if !rest.is_empty() {
        parts.push(Part::Literal(rest.into()));
    }
    Ok(parts)
}
fn render(parts: &[Part], record: &Record, elapsed: Duration) -> String {
    let mut line = String::new();
    for part in parts {
        match part {
            Part::Literal(text) => line.push_str(text),
            Part::Value(field) => escape_into(&mut line, &field.value(record, elapsed)),
        }
    }
    line
}
/// Escapes backslashes and line-breaking/control characters so one record stays one line.
pub(crate) fn escape_into(line: &mut String, value: &str) {
    for c in value.chars() {
        if c == '\\' || line_breaking(c) {
            line.extend(c.escape_debug());
        } else {
            line.push(c);
        }
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid access log configuration",
    )
}
struct Trust {
    proxies: Vec<IpNet>,
    header: HeaderName,
}
impl Trust {
    fn trusted(&self, ip: IpAddr) -> bool {
        self.proxies.iter().any(|p| p.contains(&ip))
    }
    fn resolve(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
        let mut candidate = normalize(peer?);
        let original = candidate;
        if !self.trusted(candidate) {
            return Some(candidate);
        }
        let mut values = headers.get_all(&self.header).iter();
        let Some(value) = values.next() else {
            return Some(candidate);
        };
        if values.next().is_some() || value.as_bytes().len() > 4096 {
            return Some(candidate);
        }
        let Ok(value) = value.to_str() else {
            return Some(candidate);
        };
        let entries: Vec<_> = value.split(',').collect();
        if entries.len() > 32 {
            return Some(candidate);
        }
        let Some(addresses) = entries
            .iter()
            .map(|p| p.trim().parse::<IpAddr>().ok().map(normalize))
            .collect::<Option<Vec<_>>>()
        else {
            return Some(candidate);
        };
        for address in addresses.into_iter().rev() {
            if !self.trusted(candidate) {
                break;
            }
            candidate = address;
        }
        Some(if value.is_empty() {
            original
        } else {
            candidate
        })
    }
}
fn normalize(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        _ => ip,
    }
}
#[derive(Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);
#[derive(Clone)]
pub struct RequestId(pub String);
struct Inner {
    config: Config,
    trust: Trust,
    template: Option<Vec<Part>>,
    writer: NonBlocking,
    _guard: WorkerGuard,
}
#[derive(Clone)]
pub struct AccessLog(Arc<Inner>);
impl AccessLog {
    pub fn new(config: Config) -> io::Result<Self> {
        config.validate()?;
        let writer = config.output.writer()?;
        let (writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(config.queue_capacity)
            .lossy(true)
            .finish(writer);
        Ok(Self(Arc::new(Inner {
            trust: config.trust()?,
            template: config.template()?,
            config,
            writer,
            _guard: guard,
        })))
    }
    pub fn wrap(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(self.clone(), access))
    }
    pub fn dropped_records(&self) -> usize {
        self.0.writer.error_counter().dropped_lines()
    }
}
#[derive(Serialize)]
struct Record {
    timestamp: String,
    request_id: String,
    method: String,
    route: String,
    peer_ip: Option<IpAddr>,
    client_ip: Option<IpAddr>,
    status: Option<u16>,
    outcome: &'static str,
    duration_ms: u64,
    queue_dropped_records: usize,
}
struct Pending {
    log: AccessLog,
    record: Record,
    start: Instant,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.record.duration_ms = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self.record.queue_dropped_records = self.log.dropped_records();
        let mut line = match (&self.log.0.config.format, &self.log.0.template) {
            (Format::Template, Some(parts)) => render(parts, &self.record, elapsed).into_bytes(),
            (Format::Template, None) => return,
            (Format::Json, _) => match sonic_rs::to_vec(&self.record) { Ok(bytes) => bytes, Err(_) => return },
            (Format::Text, _) => format!("{} id={} peer={} client={} method={} route={:?} status={} outcome={} duration_ms={} queue_dropped_records={}",
                self.record.timestamp, self.record.request_id,
                self.record.peer_ip.map_or_else(|| "-".into(), |p| p.to_string()),
                self.record.client_ip.map_or_else(|| "-".into(), |p| p.to_string()),
                self.record.method, self.record.route,
                self.record.status.map_or_else(|| "-".into(), |s| s.to_string()),
                self.record.outcome, self.record.duration_ms, self.record.queue_dropped_records).into_bytes(),
        };
        line.push(b'\n');
        let _ = self.log.0.writer.clone().write_all(&line);
    }
}
async fn access(State(log): State<AccessLog>, mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| normalize(p.0.ip()));
    let client = log.0.trust.resolve(peer, request.headers());
    let id = uuid::Uuid::new_v4().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", |p| p.as_str())
        .to_owned();
    let method = request.method().as_str();
    let method = if matches!(
        method,
        "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" | "CONNECT" | "TRACE"
    ) {
        method.to_owned()
    } else {
        "OTHER".into()
    };
    request.extensions_mut().insert(ClientIp(client));
    request.extensions_mut().insert(RequestId(id.clone()));
    let mut pending = Pending {
        log,
        start: Instant::now(),
        record: Record {
            timestamp: chrono::Utc::now().to_rfc3339(),
            request_id: id.clone(),
            method,
            route,
            peer_ip: peer,
            client_ip: client,
            status: None,
            outcome: "cancelled",
            duration_ms: 0,
            queue_dropped_records: 0,
        },
    };
    let mut response = next.run(request).await;
    pending.record.status = Some(response.status().as_u16());
    pending.record.outcome = "response";
    response
        .headers_mut()
        .insert("x-request-id", id.parse().expect("generated UUID header"));
    response
}

impl Output {
    pub(crate) fn writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(match self {
            Output::Stdout {} => Box::new(io::stdout()) as Box<dyn Write + Send>,
            Output::Stderr {} => Box::new(io::stderr()),
            Output::File {
                path,
                rotation,
                max_files,
            } => {
                let directory = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                let name = path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .ok_or_else(invalid)?;
                let rotation = match rotation {
                    Rotation::Never => tracing_appender::rolling::Rotation::NEVER,
                    Rotation::Hourly => tracing_appender::rolling::Rotation::HOURLY,
                    Rotation::Daily => tracing_appender::rolling::Rotation::DAILY,
                };
                Box::new(
                    tracing_appender::rolling::Builder::new()
                        .rotation(rotation)
                        .filename_prefix(name)
                        .max_log_files(*max_files)
                        .build(directory)
                        .map_err(|_| invalid())?,
                )
            }
        })
    }
}
