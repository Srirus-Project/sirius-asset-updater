//! Application-only structured events, separate from HTTP access logs.
use crate::access_log::{Format, Output};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Write},
    path::Path,
};
use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    layer::{Context, SubscriberExt},
    Layer,
};

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}
impl Level {
    fn accepts(self, level: &tracing::Level) -> bool {
        match self {
            Self::Off => false,
            Self::Error => *level == tracing::Level::ERROR,
            Self::Warn => *level <= tracing::Level::WARN,
            Self::Info => *level <= tracing::Level::INFO,
            Self::Debug => *level <= tracing::Level::DEBUG,
            Self::Trace => true,
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "off" => Self::Off,
            "error" => Self::Error,
            "warn" => Self::Warn,
            "info" => Self::Info,
            "debug" => Self::Debug,
            "trace" => Self::Trace,
            _ => return None,
        })
    }
}
/// A single level or `default[,target=level...]`; the longest matching target wins.
/// Targets match exactly or at a `::` module boundary; `filter::Targets` uses raw prefixes.
#[derive(Clone, Default, Deserialize)]
#[serde(try_from = "String")]
pub struct Levels {
    default: Level,
    targets: Vec<(String, Level)>,
}
impl From<Level> for Levels {
    fn from(default: Level) -> Self {
        Self {
            default,
            targets: vec![],
        }
    }
}
impl TryFrom<String> for Levels {
    type Error = io::Error;
    fn try_from(value: String) -> io::Result<Self> {
        value.parse()
    }
}
impl std::str::FromStr for Levels {
    type Err = io::Error;
    fn from_str(value: &str) -> io::Result<Self> {
        if value.len() > 1024 {
            return Err(invalid());
        }
        let mut items = value.split(',');
        let default = items.next().and_then(Level::parse).ok_or_else(invalid)?;
        let mut targets: Vec<(String, Level)> = vec![];
        for item in items {
            let (target, level) = item.split_once('=').ok_or_else(invalid)?;
            let level = Level::parse(level).ok_or_else(invalid)?;
            // Only application targets are ever emitted; a dependency target such as
            // `hyper` could never match, so accepting it would be a silently ignored setting.
            if !(target == APP_TARGET || target.starts_with(APP_PREFIX))
                || !target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')
                || targets.len() == 64
                || targets.iter().any(|(t, _)| t == target)
            {
                return Err(invalid());
            }
            targets.push((target.into(), level));
        }
        targets.sort_by_key(|(t, _)| std::cmp::Reverse(t.len()));
        Ok(Self { default, targets })
    }
}
impl Levels {
    fn level(&self, target: &str) -> Level {
        self.targets
            .iter()
            .find(|(t, _)| {
                target
                    .strip_prefix(t.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
            })
            .map_or(self.default, |(_, level)| *level)
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub level: Levels,
    pub format: Format,
    pub output: Output,
    pub queue_capacity: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            level: Level::Info.into(),
            format: Format::Text,
            output: Output::Stderr {},
            queue_capacity: 4096,
        }
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid application log configuration",
    )
}
impl Config {
    pub fn validate(&self) -> io::Result<()> {
        // Templates are an access-log format; application events have no placeholder set.
        if matches!(self.format, Format::Template) {
            return Err(invalid());
        }
        crate::access_log::Config {
            format: self.format.clone(),
            output: self.output.clone(),
            queue_capacity: self.queue_capacity,
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid())
    }
    /// CLI bootstrap reads only the root logging section; the command validates the full config.
    /// Reads only the `logging` section of `path` after applying `document` overrides,
    /// so an environment-supplied log setting takes effect before the command starts.
    pub fn from_file(
        path: Option<&Path>,
        document: crate::config_env::Document,
    ) -> io::Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let value: yaml_serde::Value =
            crate::config_env::load(path, document).map_err(|_| invalid())?;
        Self::from_value(&value)
    }
    /// Same as [`Self::from_file`] for document text already read (e.g. a remote snapshot),
    /// so the logging section and the command see the same bytes.
    pub fn from_text(text: &str, document: crate::config_env::Document) -> io::Result<Self> {
        let value: yaml_serde::Value =
            crate::config_env::from_str(text, document).map_err(|_| invalid())?;
        Self::from_value(&value)
    }
    fn from_value(value: &yaml_serde::Value) -> io::Result<Self> {
        let config = match value.get("logging") {
            Some(value) if !value.is_null() => {
                yaml_serde::from_value(value.clone()).map_err(|_| invalid())?
            }
            _ => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }
    pub fn init(&self) -> io::Result<WorkerGuard> {
        let (subscriber, guard) = self.subscriber()?;
        tracing::subscriber::set_global_default(subscriber).map_err(|_| invalid())?;
        Ok(guard)
    }
    pub(crate) fn subscriber(
        &self,
    ) -> io::Result<(impl Subscriber + Send + Sync + 'static, WorkerGuard)> {
        self.validate()?;
        let (writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(self.queue_capacity)
            .lossy(true)
            .finish(self.output.writer().map_err(|_| invalid())?);
        Ok((
            tracing_subscriber::registry().with(AppLayer {
                config: self.clone(),
                writer,
            }),
            guard,
        ))
    }
}
struct AppLayer {
    config: Config,
    writer: NonBlocking,
}
#[derive(Serialize)]
#[serde(untagged)]
enum Value {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Boolean(bool),
}
#[derive(Default)]
struct Fields(BTreeMap<String, Value>);
fn permitted(name: &str) -> bool {
    matches!(
        name,
        "message"
            | "event"
            | "stage"
            | "region"
            | "job_id"
            | "status"
            | "error_code"
            | "completed"
            | "failed"
            | "total"
            | "bytes"
            | "cache_hits"
            | "listen"
            | "operation"
            | "attempt"
    )
}
fn bounded(value: &str) -> String {
    let mut end = value.len().min(1024);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if permitted(field.name()) {
            self.0.insert(
                field.name().into(),
                Value::Text(bounded(&format!("{value:?}"))),
            );
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if permitted(field.name()) {
            self.0
                .insert(field.name().into(), Value::Text(bounded(value)));
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        if permitted(field.name()) {
            self.0.insert(field.name().into(), Value::Unsigned(value));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        if permitted(field.name()) {
            self.0.insert(field.name().into(), Value::Signed(value));
        }
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        if permitted(field.name()) {
            self.0.insert(field.name().into(), Value::Boolean(value));
        }
    }
}
#[derive(Serialize)]
struct Record<'a> {
    timestamp: String,
    level: &'a str,
    target: &'a str,
    queue_dropped_records: usize,
    fields: BTreeMap<String, Value>,
}
impl<S: Subscriber> Layer<S> for AppLayer {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: Context<'_, S>) -> bool {
        let target = metadata.target();
        (target == APP_TARGET || target.starts_with(APP_PREFIX))
            && self.config.level.level(target).accepts(metadata.level())
    }
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let record = Record {
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: event.metadata().level().as_str(),
            target: event.metadata().target(),
            queue_dropped_records: self.writer.error_counter().dropped_lines(),
            fields: fields.0,
        };
        let Ok(fields) = sonic_rs::to_string(&record.fields) else {
            return;
        };
        let line = match self.config.format {
            Format::Json => match sonic_rs::to_string(&record) {
                Ok(line) => line,
                Err(_) => return,
            },
            Format::Template => return,
            Format::Text => format!(
                "{} {} {} dropped={} {}",
                record.timestamp, record.level, record.target, record.queue_dropped_records, fields
            ),
        };
        let _ = writeln!(self.writer.clone(), "{line}");
    }
}
const APP_TARGET: &str = "sirius_asset_updater";
const APP_PREFIX: &str = "sirius_asset_updater::";
