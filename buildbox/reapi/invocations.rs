//! Grouping requests by the Bazel invocation that made them.
//!
//! Bazel tags every request with the ID of the invocation (one `bazel build`,
//! say) it belongs to, but doesn't propagate a trace context. So that one
//! trace shows everything the server did for an invocation, the server keeps a
//! span open for each invocation it is serving, and makes each request's span
//! a child of it.
//!
//! There is no message saying an invocation has finished, so its span ends
//! once it has made no requests for a while. A request that arrives after
//! that starts a new span, and so a new trace, for the same invocation.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::field::Empty;
use tracing::Span;

/// How long an invocation may make no requests before its span ends.
pub const INVOCATION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often to look for idle invocations.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// What a request says about the invocation that made it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvocationInfo {
    pub id: String,
    pub correlated_invocations_id: String,
    pub tool_name: String,
    pub tool_version: String,
}

/// The invocations with requests in flight or made recently.
pub struct Invocations {
    active: Mutex<HashMap<String, Invocation>>,
    idle_timeout: Duration,
}

struct Invocation {
    span: Span,
    in_flight: usize,
    requests: u64,
    last_active: Instant,
}

impl Invocation {
    fn is_idle(&self, now: Instant, idle_timeout: Duration) -> bool {
        self.in_flight == 0 && now.saturating_duration_since(self.last_active) >= idle_timeout
    }

    /// Record the invocation's totals and drop its span. The span ends once
    /// requests still in flight drop their handles to it too.
    fn end(self) {
        self.span.record("bazel.requests", self.requests);
    }
}

impl Invocations {
    pub fn new(idle_timeout: Duration) -> Arc<Invocations> {
        Arc::new(Invocations {
            active: Mutex::new(HashMap::new()),
            idle_timeout,
        })
    }

    /// Record the start of a request made by `info`'s invocation. Spans for
    /// the request should be children of the returned guard's span. Requests
    /// without an invocation ID belong to no invocation.
    pub fn begin(self: &Arc<Self>, info: &InvocationInfo) -> Option<InvocationRequest> {
        if info.id.is_empty() {
            return None;
        }

        let mut active = self.lock();
        let invocation = active.entry(info.id.clone()).or_insert_with(|| Invocation {
            span: invocation_span(info),
            in_flight: 0,
            requests: 0,
            last_active: Instant::now(),
        });
        invocation.in_flight += 1;
        invocation.requests += 1;

        Some(InvocationRequest {
            invocations: self.clone(),
            id: info.id.clone(),
            span: invocation.span.clone(),
        })
    }

    /// End the spans of invocations that have been idle for the idle timeout,
    /// as of `now`.
    pub fn close_idle(&self, now: Instant) {
        let mut idle = vec![];
        {
            let mut active = self.lock();
            let idle_ids: Vec<String> = active
                .iter()
                .filter(|(_, invocation)| invocation.is_idle(now, self.idle_timeout))
                .map(|(id, _)| id.clone())
                .collect();
            for id in idle_ids {
                idle.extend(active.remove(&id));
            }
        }
        // Ending spans exports them, so do it outside the lock.
        for invocation in idle {
            invocation.end();
        }
    }

    /// End the spans of every invocation, for example when the server stops.
    pub fn close_all(&self) {
        let active = std::mem::take(&mut *self.lock());
        for invocation in active.into_values() {
            invocation.end();
        }
    }

    /// End the spans of idle invocations as they become idle. Never returns.
    pub async fn close_idle_periodically(self: Arc<Self>) {
        let mut interval = tokio::time::interval(IDLE_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            self.close_idle(Instant::now());
        }
    }

    fn end_request(&self, id: &str) {
        let mut active = self.lock();
        let Some(invocation) = active.get_mut(id) else {
            return;
        };
        invocation.in_flight -= 1;
        invocation.last_active = Instant::now();
    }

    /// The map is never left inconsistent, so a panic while it was held
    /// doesn't make it unusable.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Invocation>> {
        self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Creates a root span, since an invocation isn't part of anything else the
/// server is doing.
fn invocation_span(info: &InvocationInfo) -> Span {
    tracing::info_span!(
        parent: None,
        "bazel_invocation",
        span.name = "bazel invocation",
        bazel.invocation_id = %info.id,
        bazel.correlated_invocations_id = %info.correlated_invocations_id,
        bazel.tool_name = %info.tool_name,
        bazel.tool_version = %info.tool_version,
        bazel.requests = Empty,
    )
}

/// A request in flight for an invocation. Dropping it marks the request as
/// finished.
pub struct InvocationRequest {
    invocations: Arc<Invocations>,
    id: String,
    span: Span,
}

impl InvocationRequest {
    /// The invocation's span.
    pub fn span(&self) -> &Span {
        &self.span
    }
}

impl Drop for InvocationRequest {
    fn drop(&mut self) {
        self.invocations.end_request(&self.id);
        // A span ends when it was last exited, rather than when it's closed.
        // The invocation's span is never entered otherwise, so entering it
        // here makes it end when the invocation's last request does, not
        // when it is found to be idle.
        self.span.in_scope(|| {});
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> InvocationInfo {
        InvocationInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    fn active_ids(invocations: &Invocations) -> Vec<String> {
        let mut ids: Vec<String> = invocations.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    #[test]
    fn requests_without_an_invocation_id_belong_to_none() {
        let invocations = Invocations::new(Duration::ZERO);

        assert!(invocations.begin(&info("")).is_none());
        assert!(active_ids(&invocations).is_empty());
    }

    #[test]
    fn requests_of_one_invocation_share_its_span() {
        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
        let invocations = Invocations::new(Duration::ZERO);

        let first = invocations.begin(&info("a")).unwrap();
        let second = invocations.begin(&info("a")).unwrap();
        let other = invocations.begin(&info("b")).unwrap();

        assert_eq!(first.span().id(), second.span().id());
        assert_ne!(first.span().id(), other.span().id());
    }

    #[test]
    fn invocations_close_only_when_idle() {
        let invocations = Invocations::new(Duration::from_secs(60));
        let busy = invocations.begin(&info("busy")).unwrap();
        drop(invocations.begin(&info("recent")).unwrap());
        drop(invocations.begin(&info("idle")).unwrap());
        invocations.lock().get_mut("idle").unwrap().last_active -= Duration::from_secs(61);

        invocations.close_idle(Instant::now() + Duration::from_secs(1));

        assert_eq!(active_ids(&invocations), ["busy", "recent"]);
        drop(busy);
    }

    #[test]
    fn requests_after_closing_start_a_new_span() {
        let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry());
        let invocations = Invocations::new(Duration::ZERO);
        let first = invocations.begin(&info("a")).unwrap();
        let first_span = first.span().clone();
        drop(first);

        invocations.close_idle(Instant::now());
        let second = invocations.begin(&info("a")).unwrap();

        assert_ne!(first_span.id(), second.span().id());
    }

    #[test]
    fn close_all_forgets_every_invocation() {
        let invocations = Invocations::new(Duration::from_secs(60));
        let request = invocations.begin(&info("a")).unwrap();

        invocations.close_all();
        drop(request);

        assert!(active_ids(&invocations).is_empty());
    }
}
