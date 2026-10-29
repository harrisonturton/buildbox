//! The task that uploads a command's events, as Bazel's
//! `BuildEventServiceUploader` does: lifecycle events before and after, and
//! the events on a stream, sent without waiting for acknowledgements but
//! each acknowledged in order. A stream that fails is opened again, with
//! backoff, and the events not yet acknowledged are sent again.

use crate::requests::{self, InvocationStatus, Lifecycle};
use crate::{Invocation, UploadOptions};
use bazel_bep::TransportError;
use bazel_bep::events::BuildEvent;
use bazel_bep::events::proto::__buffa::oneof::build_event::Payload;
use bazel_bep::events::proto::__buffa::oneof::build_event_id::Id;
use build_rust_connect::google::devtools::build::v1::{PublishBuildEventClient, PublishBuildToolEventStreamRequest};
use connectrpc::client::{CallOptions, ClientTransport};
use connectrpc::{ConnectError, ErrorCode};
use grpcclient::Channel;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// How long a lifecycle request may take.
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);

/// An event posted to the upload, and when.
pub struct Posted {
    pub event: BuildEvent,
    pub time: SystemTime,
}

/// Why an upload failed, as Bazel reports it.
#[derive(Debug)]
pub struct UploadError {
    message: String,
    /// Whether trying again later might work.
    transient: bool,
}

impl UploadError {
    fn new(message: String, status: &ConnectError) -> UploadError {
        UploadError {
            message: format!("{message} {}", describe(status)),
            transient: is_retryable(status.code),
        }
    }

    pub fn into_transport_error(self) -> TransportError {
        TransportError::Upload {
            message: self.message,
            transient: self.transient,
        }
    }
}

/// An upload's state, which its task owns.
pub struct Upload {
    client: PublishBuildEventClient<Channel>,
    invocation: Invocation,
    options: UploadOptions,
    /// The events to send, until the upload closes.
    events: mpsc::UnboundedReceiver<Posted>,
    status: InvocationStatus,
    next_sequence_number: i64,
    /// The requests sent on the stream and not yet acknowledged, in order.
    unacked: VecDeque<PublishBuildToolEventStreamRequest>,
    /// Whether the request ending the stream has been made.
    sent_last: bool,
}

/// What the task reading a stream's responses hears.
enum StreamUpdate {
    Ack(i64),
    /// The stream ended, cleanly or not.
    Closed(Result<(), ConnectError>),
}

/// How one attempt at the stream ended.
enum StreamEnd {
    /// Every event was sent and acknowledged.
    Done,
    /// The stream failed with `status`, having had `acked` events
    /// acknowledged; whether to try again is up to the status.
    Failed { status: ConnectError, acked: usize },
    /// The stream failed in a way trying again won't fix.
    Fatal(UploadError),
}

impl Upload {
    pub fn new(client: PublishBuildEventClient<Channel>, invocation: Invocation, options: UploadOptions, events: mpsc::UnboundedReceiver<Posted>) -> Upload {
        Upload {
            client,
            invocation,
            options,
            events,
            status: InvocationStatus::Unknown,
            next_sequence_number: 1,
            unacked: VecDeque::new(),
            sent_last: false,
        }
    }

    /// Upload every event posted until the upload closes, between the
    /// lifecycle events if the options ask for them.
    pub async fn run(mut self) -> Result<(), UploadError> {
        if !self.options.lifecycle_events {
            return self.publish_events().await;
        }
        let stream_start = SystemTime::now();
        self.publish_lifecycle(Lifecycle::BuildEnqueued(self.invocation.command_start)).await?;
        self.publish_lifecycle(Lifecycle::InvocationStarted(stream_start)).await?;
        let streamed = self.publish_events().await;
        let now = SystemTime::now();
        let finished = self.publish_finished(now).await;
        streamed.and(finished)
    }

    async fn publish_finished(&self, now: SystemTime) -> Result<(), UploadError> {
        self.publish_lifecycle(Lifecycle::InvocationFinished(now, self.status)).await?;
        self.publish_lifecycle(Lifecycle::BuildFinished(now, self.status)).await
    }

    /// Publish a lifecycle event, trying again with backoff while the
    /// service's errors allow.
    async fn publish_lifecycle(&self, event: Lifecycle) -> Result<(), UploadError> {
        let request = requests::lifecycle(&self.invocation, event);
        let mut attempt = 0;
        loop {
            let options = CallOptions::default().with_timeout(LIFECYCLE_TIMEOUT);
            let status = match self.client.publish_lifecycle_event_with_options(request.clone(), options).await {
                Ok(_) => return Ok(()),
                Err(status) => status,
            };
            if !should_retry(&status) {
                return Err(UploadError::new("Not retrying publishLifecycleEvent:".to_string(), &status));
            }
            if attempt == self.options.max_retries {
                return Err(UploadError::new(format!("All {attempt} retry attempts failed."), &status));
            }
            tokio::time::sleep(self.retry_delay(attempt)).await;
            attempt += 1;
        }
    }

    /// Send every event on a stream, opening it again while it fails in a
    /// way the service's errors allow trying again, until every event is
    /// acknowledged.
    async fn publish_events(&mut self) -> Result<(), UploadError> {
        let mut attempt = 0;
        loop {
            let (status, acked) = match self.stream().await {
                StreamEnd::Done => return Ok(()),
                StreamEnd::Fatal(error) => return Err(error),
                StreamEnd::Failed { status, acked } => (status, acked),
            };
            if !should_retry(&status) {
                return Err(UploadError::new("Not retrying publishBuildEvents:".to_string(), &status));
            }
            if attempt == self.options.max_retries {
                return Err(UploadError::new("Not retrying publishBuildEvents, no more attempts left:".to_string(), &status));
            }
            tokio::time::sleep(self.retry_delay(attempt)).await;
            attempt = if acked > 0 { 0 } else { attempt + 1 };
        }
    }

    /// One attempt at the stream: send what's unacknowledged from before,
    /// then each event as it's posted, and the end of the stream once the
    /// upload closes, while checking acknowledgements come in order.
    async fn stream(&mut self) -> StreamEnd {
        let stream = match self.client.publish_build_tool_event_stream().await {
            Ok(stream) => stream,
            Err(status) => return StreamEnd::Failed { status, acked: 0 },
        };
        let (mut sender, receiver) = stream.into_split();
        let (updates, mut updated) = mpsc::unbounded_channel();
        let reader = AbortOnDrop(tokio::spawn(read_acks(receiver, updates)));
        for request in &self.unacked {
            let _ = sender.send(request.clone()).await;
        }
        if self.sent_last {
            sender.close_send();
        }
        let mut acked = 0;
        loop {
            tokio::select! {
                posted = self.events.recv(), if !self.sent_last => {
                    let request = self.next_request(posted);
                    self.unacked.push_back(request.clone());
                    let _ = sender.send(request).await;
                    if self.sent_last {
                        sender.close_send();
                    }
                }
                update = updated.recv() => {
                    let update = update.unwrap_or(StreamUpdate::Closed(Ok(())));
                    match self.on_update(update) {
                        Ok(()) => acked += 1,
                        Err(end) => {
                            drop(reader);
                            return end.with_acked(acked);
                        }
                    }
                }
            }
        }
    }

    /// The request for the next event, or the end of the stream if the
    /// upload has closed.
    fn next_request(&mut self, posted: Option<Posted>) -> PublishBuildToolEventStreamRequest {
        let sequence_number = self.next_sequence_number;
        self.next_sequence_number += 1;
        let Some(posted) = posted else {
            self.sent_last = true;
            return requests::stream_finished(&self.invocation, sequence_number, SystemTime::now());
        };
        if let Some(status) = status_of(&posted.event) {
            self.status = status;
        }
        requests::bazel_event(&self.invocation, sequence_number, posted.time, &posted.event)
    }

    /// Take an acknowledgement, which must be of the oldest request not yet
    /// acknowledged, or the stream's end.
    fn on_update(&mut self, update: StreamUpdate) -> Result<(), StreamEnd> {
        let ack = match update {
            StreamUpdate::Ack(ack) => ack,
            StreamUpdate::Closed(result) => return Err(self.closed(result)),
        };
        let Some(expected) = self.unacked.front() else {
            let message = format!("Received ACK (seqNum={ack}) when no ACK was expected");
            return Err(StreamEnd::failed(ConnectError::failed_precondition(message)));
        };
        let expected = requests::sequence_number(expected);
        if expected != ack {
            let message = format!("Expected ACK with seqNum={expected} but received ACK with seqNum={ack}");
            return Err(StreamEnd::failed(ConnectError::failed_precondition(message)));
        }
        self.unacked.pop_front();
        Ok(())
    }

    /// How the stream ended when the service closed it.
    fn closed(&self, result: Result<(), ConnectError>) -> StreamEnd {
        let complete = self.sent_last && self.unacked.is_empty();
        match (result, complete) {
            (Ok(()), true) => StreamEnd::Done,
            (Ok(()), false) => StreamEnd::failed(ConnectError::failed_precondition(self.incomplete_message())),
            (Err(status), true) => StreamEnd::Fatal(UploadError::new("The stream completed with an error after every event was acknowledged:".to_string(), &status)),
            (Err(status), false) => StreamEnd::failed(status),
        }
    }

    fn incomplete_message(&self) -> String {
        match self.sent_last {
            true => format!("Server closed stream with status OK but not all ACKs have been received (ackQueue={})", self.unacked.len()),
            false => "Server closed stream with status OK but not all events have been sent".to_string(),
        }
    }

    fn retry_delay(&self, attempt: u32) -> Duration {
        self.options.retry_delay.mul_f64(1.6f64.powi(attempt as i32))
    }
}

impl StreamEnd {
    fn failed(status: ConnectError) -> StreamEnd {
        StreamEnd::Failed { status, acked: 0 }
    }

    fn with_acked(self, acked: usize) -> StreamEnd {
        match self {
            StreamEnd::Failed { status, .. } => StreamEnd::Failed { status, acked },
            end => end,
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type ResponseBody = <Channel as ClientTransport>::ResponseBody;
type AckView = build_rust_connect::google::devtools::build::v1::__buffa::view::PublishBuildToolEventStreamResponseView<'static>;

/// Pass on each acknowledgement the service sends, then how the stream
/// ended.
async fn read_acks(mut receiver: connectrpc::client::BidiRecvHalf<ResponseBody, AckView>, updates: mpsc::UnboundedSender<StreamUpdate>) {
    loop {
        let update = match receiver.message().await {
            Ok(Some(ack)) => StreamUpdate::Ack(ack.view().sequence_number),
            Ok(None) => StreamUpdate::Closed(Ok(())),
            Err(status) => StreamUpdate::Closed(Err(status)),
        };
        let closed = matches!(update, StreamUpdate::Closed(_));
        if updates.send(update).is_err() || closed {
            return;
        }
    }
}

/// How the command went, if the event says: the build finished event, or
/// that event aborted.
fn status_of(event: &BuildEvent) -> Option<InvocationStatus> {
    match &event.payload {
        Some(Payload::Finished(finished)) => {
            let code = finished.exit_code.as_option().map_or(0, |code| code.code);
            Some(if code == 0 { InvocationStatus::Succeeded } else { InvocationStatus::Failed })
        }
        Some(Payload::Aborted(_)) => {
            let id = event.id.as_option().and_then(|id| id.id.as_ref());
            matches!(id, Some(Id::BuildFinished(_))).then_some(InvocationStatus::Failed)
        }
        _ => None,
    }
}

/// Whether a request that failed with `status` may be tried again: not if
/// the request was bad, or the service says the invocation must start
/// over.
fn should_retry(status: &ConnectError) -> bool {
    is_retryable(status.code) && status.code != ErrorCode::FailedPrecondition
}

fn is_retryable(code: ErrorCode) -> bool {
    !matches!(code, ErrorCode::InvalidArgument | ErrorCode::PermissionDenied)
}

/// A status as Bazel shows it: its code, its message and what caused it.
fn describe(status: &ConnectError) -> String {
    let mut text = status.code.as_str().to_uppercase();
    if let Some(message) = &status.message {
        text.push_str(": ");
        text.push_str(message);
    }
    let mut source = std::error::Error::source(status);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
