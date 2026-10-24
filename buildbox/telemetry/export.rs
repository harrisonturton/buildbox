use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::context::{SpanContext, SpanId};
use crate::layer::{Kind, Value};
use crate::otlp_json::OtlpJsonFile;

/// The most finished spans kept while they wait to be written; more are
/// dropped.
const CAPACITY: usize = 65536;

/// How often it writes what's finished.
const INTERVAL: Duration = Duration::from_secs(5);

static EXPORTING: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<VecDeque<Finished>> = Mutex::new(VecDeque::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// A span that's ended, waiting to be written.
pub(crate) struct Finished {
    pub(crate) context: SpanContext,
    pub(crate) parent: Option<SpanId>,
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) started: SystemTime,
    pub(crate) ended: SystemTime,
    pub(crate) attributes: Vec<(&'static str, Value)>,
    pub(crate) links: Vec<SpanContext>,
    pub(crate) error: Option<String>,
}

/// Whether spans are being written anywhere, and so recorded.
pub(crate) fn exporting() -> bool {
    EXPORTING.load(Ordering::Relaxed)
}

/// Queues `span` to be written, or drops it if the queue's full.
pub(crate) fn finish(span: Finished) {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() >= CAPACITY {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    queue.push_back(span);
}

/// Where spans are written, shared by the periodic writer and the final
/// flush.
#[derive(Clone)]
pub(crate) struct Writer {
    file: Arc<Mutex<OtlpJsonFile>>,
}

/// Starts recording spans and writing them to `file` every few seconds, on
/// the tokio runtime it's called on. Only once.
pub(crate) fn start(file: OtlpJsonFile) -> Result<Writer, crate::TelemetryError> {
    if EXPORTING.swap(true, Ordering::Relaxed) {
        return Err(crate::TelemetryError::Started);
    }
    let writer = Writer {
        file: Arc::new(Mutex::new(file)),
    };
    tokio::spawn(run(writer.clone()));
    Ok(writer)
}

async fn run(writer: Writer) {
    let mut tick = tokio::time::interval(INTERVAL);
    loop {
        tick.tick().await;
        writer.flush().await;
    }
}

impl Writer {
    /// Write every span finished so far.
    pub(crate) async fn flush(&self) {
        let batch = take(usize::MAX);
        if !batch.is_empty() {
            let file = self.file.clone();
            let written = tokio::task::spawn_blocking(move || {
                let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
                file.write(batch)
            })
            .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(err)) => tracing::warn!("writing spans failed: {err}"),
                Err(err) => tracing::warn!("writing spans failed: {err}"),
            }
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            tracing::warn!(dropped, "dropped spans, finished faster than they could be written");
        }
    }
}

/// Turns recording on without writing anything.
#[cfg(test)]
pub(crate) fn record_for_tests() {
    EXPORTING.store(true, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn take_all() -> Vec<Finished> {
    take(usize::MAX)
}

fn take(count: usize) -> Vec<Finished> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let count = count.min(queue.len());
    queue.drain(..count).collect()
}
