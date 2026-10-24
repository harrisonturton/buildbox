//! Logging and tracing.
//!
//! Log events are written to stderr. Spans, which record how long each part of
//! the server's work takes, are the `tracing` spans at `info` or above (or
//! as `TRACE_LEVEL` says), written as OTLP JSON to a trace file once
//! [`Telemetry::init`] is given one; before that, or without one, nothing is
//! recorded. Sending them to a collector instead only needs another writer in
//! `export`.
//!
//! A span is part of the trace of the span it's inside, or starts a trace of
//! its own. A few fields mean something here rather than being kept as
//! attributes:
//!
//! - [`TRACEPARENT`], a W3C `traceparent` header's value, continues the
//!   trace another process started, as a server's span for a call does.
//! - [`NAME`] names the span, in place of the name `tracing` needs fixed.
//! - [`KIND`] says whether it's `"server"`, serving a call, or `"client"`.
//! - [`ERROR`] marks it failed, with why; so does an `error!` inside it.
//! - [`NESTED`], `true`, records it only inside a trace, never starting
//!   one: for work done often on its own, such as a store call, that's of
//!   interest only as part of something larger.
//! - [`LINK`], another `traceparent`, links it to a span in another trace
//!   that caused it. Recording it again adds another.
//!
//! ```ignore
//! let span = tracing::info_span!(
//!     "rpc",
//!     span.name = %procedure,
//!     span.kind = "server",
//!     traceparent = headers.get(telemetry::HEADER).and_then(|v| v.to_str().ok()).unwrap_or(""),
//! );
//! handle(request).instrument(span).await
//! ```
//!
//! A span ends when it is last exited, rather than when it's closed, so a
//! span kept open by a handle elsewhere still ends when its work did.
//!
//! Spans are written every few seconds, or dropped if more finish than can be
//! written; tracing never holds up the work it records.

mod context;
mod export;
mod layer;
mod otlp_json;

use std::path::{Path, PathBuf};

use tracing::Subscriber;
use tracing_subscriber::filter::{filter_fn, LevelFilter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer as _};

pub use context::{SpanContext, SpanId, TraceId};
pub use layer::Value;

/// The header a call carries its trace in.
pub const HEADER: &str = "traceparent";

/// The field that continues a trace from a `traceparent` header's value.
pub const TRACEPARENT: &str = "traceparent";

/// The field that names a span.
pub const NAME: &str = "span.name";

/// The field that says a span's kind: `"server"` or `"client"`.
pub const KIND: &str = "span.kind";

/// The field that marks a span failed, with why.
pub const ERROR: &str = "error";

/// The field that links a span to another trace's, by its `traceparent`.
pub const LINK: &str = "link";

/// The field that, `true`, keeps a span from starting a trace.
pub const NESTED: &str = "span.nested";

const SERVICE_NAME: &str = "buildbox";

/// The environment variable that sets which log events are written, e.g.
/// LOG_LEVEL=debug.
const LOG_LEVEL_ENV: &str = "LOG_LEVEL";

/// The environment variable that sets which spans are recorded, e.g.
/// TRACE_LEVEL=debug to include each blob the CAS reads and writes.
const TRACE_LEVEL_ENV: &str = "TRACE_LEVEL";

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("failed to open the trace file {}: {error}", path.display())]
    OpenTraceFile { path: PathBuf, error: std::io::Error },

    #[error("spans are already being written")]
    Started,
}

/// Write log events to stderr, and record no spans.
///
/// Sets the global tracing subscriber, so may only be called once.
pub fn init_logging() {
    tracing_subscriber::registry().with(log_layer()).init();
}

/// Writes spans until shut down.
pub struct Telemetry {
    writer: Option<export::Writer>,
}

impl Telemetry {
    /// Write log events to stderr and, if `trace_file` is given, append spans
    /// to it as OTLP JSON, on the tokio runtime this is called on.
    ///
    /// Sets the global tracing subscriber, so may only be called once, and
    /// not as well as [`init_logging`].
    pub fn init(trace_file: Option<&Path>) -> Result<Telemetry, TelemetryError> {
        let writer = match trace_file {
            Some(path) => Some(start_writing(path)?),
            None => None,
        };
        tracing_subscriber::registry()
            .with(log_layer())
            .with(span_layer())
            .init();
        Ok(Telemetry { writer })
    }

    /// Write the spans that have finished but not been written yet.
    pub async fn shutdown(self) {
        if let Some(writer) = self.writer {
            writer.flush().await;
        }
    }
}

fn start_writing(path: &Path) -> Result<export::Writer, TelemetryError> {
    let file = otlp_json::OtlpJsonFile::open(path, SERVICE_NAME, env!("CARGO_PKG_VERSION")).map_err(|error| {
        TelemetryError::OpenTraceFile {
            path: path.to_path_buf(),
            error,
        }
    })?;
    export::start(file)
}

/// Add an attribute to the span this code runs in, if it's being recorded.
///
/// Unlike recording a field, this works for attributes the code that made
/// the span didn't declare, such as what a request handler finds out about
/// the request its span was made for.
pub fn set_attribute(key: &'static str, value: impl Into<Value>) {
    let value = value.into();
    with_current_recording(|recording| recording.attributes.push((key, value)));
}

/// Run `update` on the span this code runs in, if it's being recorded.
fn with_current_recording(update: impl FnOnce(&mut layer::Recording)) {
    tracing::Span::current().with_subscriber(|(id, dispatch)| {
        let Some(registry) = dispatch.downcast_ref::<tracing_subscriber::Registry>() else {
            return;
        };
        let Some(span) = registry.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(recording) = extensions.get_mut::<layer::Recording>() {
            update(recording);
        }
    });
}

/// Mark the span this code runs in failed, with why, if it's being recorded.
/// Like an `error!` inside it, but for failures that aren't the server's,
/// such as an action that couldn't run, which shouldn't be logged as errors.
pub fn set_error(message: String) {
    with_current_recording(|recording| recording.error = Some(message));
}

/// The layer that records spans.
fn span_layer<S: Subscriber + for<'a> LookupSpan<'a>>() -> impl tracing_subscriber::Layer<S> {
    layer::Layer.with_filter(level_filter(TRACE_LEVEL_ENV))
}

/// Log events to stderr. Spans are left out of this layer, so log lines
/// aren't prefixed with every span they happened in.
fn log_layer<S: Subscriber + for<'a> LookupSpan<'a>>() -> impl tracing_subscriber::Layer<S> {
    let events_only = filter_fn(|metadata| metadata.is_event());
    tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(events_only)
        .with_filter(level_filter(LOG_LEVEL_ENV))
}

/// Filter by the directives in the environment variable `var`, or at info by
/// default.
fn level_filter(var: &str) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var(var)
        .from_env_lossy()
}

#[cfg(test)]
mod tests;
