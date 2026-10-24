use std::fmt;
use std::time::{Instant, SystemTime};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::context::{SpanContext, SpanId, TraceId};
use crate::export::{self, Finished};

/// What a span's kind is, from its `span.kind` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Internal,
    Server,
    Client,
}

/// An attribute's value.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Float(f64),
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Str(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::Str(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Int(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Value::Int(value.into())
    }
}

/// Counts too large for an `i64` are kept as strings, as fields are.
impl From<usize> for Value {
    fn from(value: usize) -> Self {
        match i64::try_from(value) {
            Ok(value) => Value::Int(value),
            Err(_) => Value::Str(value.to_string()),
        }
    }
}

impl From<u64> for Value {
    fn from(value: u64) -> Self {
        match i64::try_from(value) {
            Ok(value) => Value::Int(value),
            Err(_) => Value::Str(value.to_string()),
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

/// A span being recorded, kept in the registry's extensions for it.
pub(crate) struct Recording {
    pub(crate) context: SpanContext,
    parent: Option<SpanId>,
    name: String,
    kind: Kind,
    started: SystemTime,
    started_at: Instant,
    last_exited: Option<Instant>,
    pub(crate) attributes: Vec<(&'static str, Value)>,
    links: Vec<SpanContext>,
    pub(crate) error: Option<String>,
}

/// Records the spans of the subscriber it's part of; see the crate's docs.
pub(crate) struct Layer;

impl<S: Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for Layer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !export::exporting() {
            return;
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let parent = match attrs.parent() {
            Some(parent) => ctx.span(parent),
            None if attrs.is_contextual() => ctx.lookup_current(),
            None => None,
        };
        let traced = parent.and_then(|parent| {
            parent
                .scope()
                .find_map(|span| span.extensions().get::<Recording>().map(|r| r.context))
        });
        let remote = fields
            .traceparent
            .as_deref()
            .and_then(SpanContext::from_traceparent);
        let (trace, parent, sampled) = match remote.or(traced) {
            Some(parent) => (parent.trace, Some(parent.span), parent.sampled),
            None => (TraceId::random(), None, !fields.nested),
        };
        let recording = Recording {
            context: SpanContext {
                trace,
                span: SpanId::random(),
                sampled,
            },
            parent,
            name: fields
                .name
                .unwrap_or_else(|| attrs.metadata().name().to_owned()),
            kind: fields.kind.unwrap_or(Kind::Internal),
            started: SystemTime::now(),
            started_at: Instant::now(),
            last_exited: None,
            attributes: fields.attributes,
            links: fields.links,
            error: fields.error,
        };
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(recording);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(recording) = extensions.get_mut::<Recording>() else {
            return;
        };
        let mut fields = Fields::default();
        values.record(&mut fields);
        recording.attributes.extend(fields.attributes);
        recording.links.extend(fields.links);
        if fields.error.is_some() {
            recording.error = fields.error;
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if *event.metadata().level() != Level::ERROR {
            return;
        }
        let Some(span) = ctx.event_span(event) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        let Some(recording) = extensions.get_mut::<Recording>() else {
            return;
        };
        let mut message = Message::default();
        event.record(&mut message);
        recording.error.get_or_insert(message.0);
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(recording) = extensions.get_mut::<Recording>() {
            recording.last_exited = Some(Instant::now());
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(recording) = span.extensions_mut().remove::<Recording>() else {
            return;
        };
        if !recording.context.sampled {
            return;
        }
        // A span ends when it was last exited, if it ever was, rather than
        // when the last handle to it is dropped, which may be much later.
        let ended_at = recording.last_exited.unwrap_or_else(Instant::now);
        export::finish(Finished {
            context: recording.context,
            parent: recording.parent,
            name: recording.name,
            kind: recording.kind,
            started: recording.started,
            ended: recording.started + ended_at.duration_since(recording.started_at),
            attributes: recording.attributes,
            links: recording.links,
            error: recording.error,
        });
    }
}

/// A span's fields: the ones that say where it sits and what it's called,
/// and the rest as attributes.
#[derive(Default)]
struct Fields {
    traceparent: Option<String>,
    name: Option<String>,
    kind: Option<Kind>,
    error: Option<String>,
    links: Vec<SpanContext>,
    nested: bool,
    attributes: Vec<(&'static str, Value)>,
}

impl Fields {
    fn add(&mut self, field: &Field, value: Value) {
        match (field.name(), value) {
            (crate::TRACEPARENT, Value::Str(value)) => self.traceparent = Some(value),
            (crate::NAME, Value::Str(value)) => self.name = Some(value),
            (crate::KIND, Value::Str(value)) => {
                self.kind = match value.as_str() {
                    "server" => Some(Kind::Server),
                    "client" => Some(Kind::Client),
                    _ => Some(Kind::Internal),
                }
            }
            (crate::ERROR, Value::Str(value)) => self.error = Some(value),
            (crate::NESTED, Value::Bool(value)) => self.nested = value,
            (crate::LINK, Value::Str(value)) => {
                self.links.extend(SpanContext::from_traceparent(&value));
            }
            (name, value) => self.attributes.push((name, value)),
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, Value::Str(value.to_owned()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.add(field, Value::Int(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match i64::try_from(value) {
            Ok(value) => self.add(field, Value::Int(value)),
            Err(_) => self.add(field, Value::Str(value.to_string())),
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.add(field, Value::Bool(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.add(field, Value::Float(value));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.add(field, Value::Str(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.add(field, Value::Str(format!("{value:?}")));
    }
}

/// An event's message.
#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}
