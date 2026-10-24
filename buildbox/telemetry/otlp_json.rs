//! Writes spans to a file in the OTLP JSON format.
//!
//! Each line of the file is one `ExportTraceServiceRequest`, as specified for
//! the OpenTelemetry file exporter. The OpenTelemetry Collector's
//! `otlpjsonfile` receiver can read it, to send the spans on to any tracing
//! backend, and `jq` can query it directly.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::context::SpanContext;
use crate::export::Finished;
use crate::layer::{Kind, Value};

/// Appends batches of spans to a file, each as one line of OTLP JSON.
pub(crate) struct OtlpJsonFile {
    file: BufWriter<File>,
    resource: Vec<JsonKeyValue>,
}

impl OtlpJsonFile {
    /// Append to the file at `path`, creating it if needed. Spans are
    /// described as being from `service`, at `version`.
    pub(crate) fn open(path: &Path, service: &str, version: &str) -> std::io::Result<OtlpJsonFile> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let resource = vec![
            key_value("service.name", Value::Str(service.to_owned())),
            key_value("service.version", Value::Str(version.to_owned())),
        ];
        Ok(OtlpJsonFile {
            file: BufWriter::new(file),
            resource,
        })
    }

    /// Write `spans` as one line. This blocks.
    pub(crate) fn write(&mut self, spans: Vec<Finished>) -> Result<(), String> {
        let request = export_request(&self.resource, spans);
        let mut line = serde_json::to_vec(&request).map_err(|err| format!("encoding spans: {err}"))?;
        line.push(b'\n');
        self.file
            .write_all(&line)
            .and_then(|()| self.file.flush())
            .map_err(|err| err.to_string())
    }
}

// The OTLP JSON encoding: protobuf's JSON mapping of the OTLP messages, with
// trace and span IDs as hex strings. 64-bit integers are strings, as protobuf's
// JSON mapping requires.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportTraceServiceRequest {
    resource_spans: Vec<ResourceSpans>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans {
    resource: Resource,
    scope_spans: Vec<ScopeSpans>,
}

#[derive(Serialize)]
struct Resource {
    attributes: Vec<JsonKeyValue>,
}

#[derive(Serialize)]
struct ScopeSpans {
    scope: Scope,
    spans: Vec<JsonSpan>,
}

#[derive(Serialize)]
struct Scope {
    name: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonSpan {
    trace_id: String,
    span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_span_id: Option<String>,
    name: String,
    kind: i32,
    start_time_unix_nano: String,
    end_time_unix_nano: String,
    attributes: Vec<JsonKeyValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    links: Vec<JsonLink>,
    status: Status,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonLink {
    trace_id: String,
    span_id: String,
}

#[derive(Default, Serialize)]
struct Status {
    code: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,
}

#[derive(Clone, Serialize)]
struct JsonKeyValue {
    key: String,
    value: AnyValue,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum AnyValue {
    StringValue(String),
    BoolValue(bool),
    IntValue(String),
    DoubleValue(f64),
}

/// OTLP's `Status.StatusCode` for a failed span.
const STATUS_CODE_ERROR: i32 = 2;

fn export_request(resource: &[JsonKeyValue], spans: Vec<Finished>) -> ExportTraceServiceRequest {
    let scope_spans = ScopeSpans {
        scope: Scope { name: "buildbox" },
        spans: spans.into_iter().map(span).collect(),
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Resource {
                attributes: resource.to_vec(),
            },
            scope_spans: vec![scope_spans],
        }],
    }
}

fn span(span: Finished) -> JsonSpan {
    // OTLP's `Span.SpanKind`.
    let kind = match span.kind {
        Kind::Internal => 1,
        Kind::Server => 2,
        Kind::Client => 3,
    };
    let status = match span.error {
        Some(message) => Status {
            code: STATUS_CODE_ERROR,
            message,
        },
        None => Status::default(),
    };
    JsonSpan {
        trace_id: span.context.trace.to_string(),
        span_id: span.context.span.to_string(),
        parent_span_id: span.parent.map(|parent| parent.to_string()),
        name: span.name,
        kind,
        start_time_unix_nano: nanos(span.started),
        end_time_unix_nano: nanos(span.ended),
        attributes: span.attributes.into_iter().map(|(key, value)| key_value(key, value)).collect(),
        links: span.links.iter().map(link).collect(),
        status,
    }
}

fn link(link: &SpanContext) -> JsonLink {
    JsonLink {
        trace_id: link.trace.to_string(),
        span_id: link.span.to_string(),
    }
}

fn key_value(key: &str, value: Value) -> JsonKeyValue {
    let value = match value {
        Value::Str(v) => AnyValue::StringValue(v),
        Value::Int(v) => AnyValue::IntValue(v.to_string()),
        Value::Bool(v) => AnyValue::BoolValue(v),
        Value::Float(v) => AnyValue::DoubleValue(v),
    };
    JsonKeyValue {
        key: key.to_owned(),
        value,
    }
}

/// A time as nanoseconds since the Unix epoch. Times before the epoch are
/// reported as the epoch.
fn nanos(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    since_epoch.as_nanos().to_string()
}
