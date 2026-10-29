//! Streaming a command's build events to a Build Event Service, as Bazel's
//! `--bes_backend` does: lifecycle events around the invocation, and the
//! events themselves on a stream whose every message the service
//! acknowledges.

mod requests;
mod upload;

pub use requests::{BAZEL_EVENT_TYPE, InvocationStatus};

use async_trait::async_trait;
use bazel_bep::events::BuildEvent;
use bazel_bep::{Transport, TransportError};
use build_rust_connect::google::devtools::build::v1::PublishBuildEventClient;
use grpcclient::{Channel, Target, TargetError, TlsError};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use upload::{Posted, Upload, UploadError};

/// What the service is told of a command besides its events.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// The build the command is an attempt at, which Bazel calls the build
    /// request ID.
    pub build_id: String,
    pub invocation_id: String,
    pub attempt_number: i64,
    /// When the command started, which the build's enqueued event reports.
    pub command_start: SystemTime,
    /// The notification keywords the stream's first request carries.
    pub keywords: Vec<String>,
    /// The instance the service keeps the events under:
    /// `--bes_instance_name`.
    pub project_id: String,
    /// `--bes_check_preceding_lifecycle_events`.
    pub check_preceding_lifecycle_events: bool,
}

/// How an upload goes.
#[derive(Clone, Debug)]
pub struct UploadOptions {
    /// Whether to publish lifecycle events: `--bes_lifecycle_events`.
    pub lifecycle_events: bool,
    /// Whether closing waits for the upload to finish.
    pub wait: bool,
    /// How long closing waits, or `None` for as long as it takes:
    /// `--bes_timeout`.
    pub timeout: Option<Duration>,
    /// How many times a failed request is tried again.
    pub max_retries: u32,
    /// How long to wait before the first retry. Each wait after is 1.6
    /// times longer.
    pub retry_delay: Duration,
}

impl Default for UploadOptions {
    /// Bazel's defaults.
    fn default() -> UploadOptions {
        UploadOptions {
            lifecycle_events: true,
            wait: true,
            timeout: None,
            max_retries: 4,
            retry_delay: Duration::from_secs(1),
        }
    }
}

/// Why a Build Event Service can't be used.
#[derive(Debug, thiserror::Error)]
pub enum BesError {
    #[error(transparent)]
    Target(#[from] TargetError),

    #[error(transparent)]
    Tls(#[from] TlsError),

    #[error("invalid header '{name}: {value}'")]
    Header { name: String, value: String },
}

/// A client of a Build Event Service.
#[derive(Clone)]
pub struct BuildEventService {
    client: PublishBuildEventClient<Channel>,
}

impl BuildEventService {
    /// A client of the service at `url`, trusting the certificates in the
    /// PEM file `certificates` as well as Mozilla's, and sending `headers`
    /// with every request; a later header replaces an earlier one of the
    /// same name.
    pub fn new(url: &str, certificates: Option<&Path>, headers: &[(String, String)]) -> Result<BuildEventService, BesError> {
        let target = Target::parse(url)?;
        let channel = Channel::new(&target, certificates)?;
        let mut header_map = http::HeaderMap::new();
        for (name, value) in headers {
            let (name, value) = header(name, value).ok_or_else(|| BesError::Header { name: name.clone(), value: value.clone() })?;
            header_map.insert(name, value);
        }
        let config = channel.config().with_default_headers(header_map);
        Ok(BuildEventService {
            client: PublishBuildEventClient::new(channel, config),
        })
    }

    /// Start uploading a command's events. Those posted to the upload go to
    /// the service in order.
    pub fn upload(&self, invocation: Invocation, options: UploadOptions) -> BesUpload {
        let (events, received) = mpsc::unbounded_channel();
        let upload = Upload::new(self.client.clone(), invocation, options.clone(), received);
        BesUpload {
            events: Mutex::new(Some(events)),
            task: Mutex::new(Some(tokio::spawn(upload.run()))),
            options,
        }
    }
}

fn header(name: &str, value: &str) -> Option<(http::HeaderName, http::HeaderValue)> {
    let name = http::HeaderName::try_from(name).ok()?;
    let value = http::HeaderValue::try_from(value).ok()?;
    Some((name, value))
}

/// A command's events on their way to a Build Event Service, sent by a
/// task of their own so that posting never waits on the network.
pub struct BesUpload {
    /// Where events are sent to be uploaded, until the upload closes.
    events: Mutex<Option<mpsc::UnboundedSender<Posted>>>,
    task: Mutex<Option<JoinHandle<Result<(), UploadError>>>>,
    options: UploadOptions,
}

#[async_trait]
impl Transport for BesUpload {
    fn post(&self, event: BuildEvent) {
        let events = self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(events) = events.as_ref() {
            let _ = events.send(Posted { event, time: SystemTime::now() });
        }
    }

    /// End the stream after the events posted, and wait for the service to
    /// acknowledge them and the invocation's end, unless the options say
    /// not to wait, or not so long.
    async fn close(&self) -> Result<(), TransportError> {
        self.events.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        let task = self.task.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        let Some(mut task) = task else {
            return Ok(());
        };
        if !self.options.wait {
            return Ok(());
        }
        let uploaded = match self.options.timeout {
            Some(timeout) => match tokio::time::timeout(timeout, &mut task).await {
                Ok(uploaded) => uploaded,
                Err(_) => {
                    task.abort();
                    return Err(TransportError::Timeout);
                }
            },
            None => task.await,
        };
        match uploaded {
            Ok(result) => result.map_err(UploadError::into_transport_error),
            Err(error) => Err(TransportError::Upload { message: error.to_string(), transient: true }),
        }
    }
}

#[cfg(test)]
mod tests;
