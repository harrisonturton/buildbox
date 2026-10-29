//! The requests a command's upload makes to a Build Event Service, as
//! Bazel's `BuildEventServiceProtoUtil` makes them.

use crate::Invocation;
use buffa::{EnumValue, Message, MessageField};
use buffa_types::google::protobuf::{Any, Timestamp};
use build_rust_connect::google::devtools::build::v1::__buffa::oneof::build_event::Event;
use build_rust_connect::google::devtools::build::v1::build_event::build_component_stream_finished::FinishType;
use build_rust_connect::google::devtools::build::v1::build_event::{
    BuildComponentStreamFinished, BuildEnqueued, BuildFinished, InvocationAttemptFinished, InvocationAttemptStarted,
};
use build_rust_connect::google::devtools::build::v1::build_status::Result as BuildResult;
use build_rust_connect::google::devtools::build::v1::publish_lifecycle_event_request::ServiceLevel;
use build_rust_connect::google::devtools::build::v1::stream_id::BuildComponent;
use build_rust_connect::google::devtools::build::v1::{
    BuildEvent, BuildStatus, OrderedBuildEvent, PublishBuildToolEventStreamRequest, PublishLifecycleEventRequest, StreamId,
};
use std::time::SystemTime;

/// The type of the Build Event Protocol's events, packed in each request.
pub const BAZEL_EVENT_TYPE: &str = "type.googleapis.com/build_event_stream.BuildEvent";

/// How a command ended, as lifecycle events report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationStatus {
    Unknown,
    Succeeded,
    Failed,
}

/// The four events around a command's stream, which the service hears of
/// apart from it.
#[derive(Clone, Copy, Debug)]
pub enum Lifecycle {
    BuildEnqueued(SystemTime),
    InvocationStarted(SystemTime),
    InvocationFinished(SystemTime, InvocationStatus),
    BuildFinished(SystemTime, InvocationStatus),
}

/// The request reporting a lifecycle event.
pub fn lifecycle(invocation: &Invocation, event: Lifecycle) -> PublishLifecycleEventRequest {
    let (sequence_number, time, payload): (i64, SystemTime, Event) = match event {
        Lifecycle::BuildEnqueued(time) => (1, time, BuildEnqueued::default().into()),
        Lifecycle::InvocationStarted(time) => (1, time, invocation_started(invocation).into()),
        Lifecycle::InvocationFinished(time, status) => (2, time, invocation_finished(status).into()),
        Lifecycle::BuildFinished(time, status) => (2, time, build_finished(status).into()),
    };
    let keywords = match payload {
        Event::InvocationAttemptFinished(_) => Vec::new(),
        _ => invocation.keywords.clone(),
    };
    let stream_id = stream_id(invocation, &payload);
    PublishLifecycleEventRequest {
        service_level: EnumValue::from(ServiceLevel::INTERACTIVE),
        build_event: MessageField::some(OrderedBuildEvent {
            stream_id: MessageField::some(stream_id),
            sequence_number,
            event: MessageField::some(BuildEvent {
                event_time: MessageField::some(Timestamp::from(time)),
                event: Some(payload),
                ..Default::default()
            }),
            ..Default::default()
        }),
        notification_keywords: keywords,
        project_id: invocation.project_id.clone(),
        ..Default::default()
    }
}

fn invocation_started(invocation: &Invocation) -> InvocationAttemptStarted {
    InvocationAttemptStarted {
        attempt_number: invocation.attempt_number,
        ..Default::default()
    }
}

fn invocation_finished(status: InvocationStatus) -> InvocationAttemptFinished {
    InvocationAttemptFinished {
        invocation_status: MessageField::some(build_status(status)),
        ..Default::default()
    }
}

fn build_finished(status: InvocationStatus) -> BuildFinished {
    BuildFinished {
        status: MessageField::some(build_status(status)),
        ..Default::default()
    }
}

fn build_status(status: InvocationStatus) -> BuildStatus {
    let result = match status {
        InvocationStatus::Unknown => BuildResult::UNKNOWN_STATUS,
        InvocationStatus::Succeeded => BuildResult::COMMAND_SUCCEEDED,
        InvocationStatus::Failed => BuildResult::COMMAND_FAILED,
    };
    BuildStatus {
        result: EnumValue::from(result),
        ..Default::default()
    }
}

/// The request carrying the `sequence_number`th build event of the stream.
pub fn bazel_event(invocation: &Invocation, sequence_number: i64, time: SystemTime, event: &bazel_bep::events::BuildEvent) -> PublishBuildToolEventStreamRequest {
    let packed = Any {
        type_url: BAZEL_EVENT_TYPE.to_string(),
        value: event.encode_to_vec().into(),
        ..Default::default()
    };
    stream_request(invocation, sequence_number, time, Event::BazelEvent(Box::new(packed)))
}

/// The request that ends the stream, after its last build event.
pub fn stream_finished(invocation: &Invocation, sequence_number: i64, time: SystemTime) -> PublishBuildToolEventStreamRequest {
    let finished = BuildComponentStreamFinished {
        r#type: EnumValue::from(FinishType::FINISHED),
        ..Default::default()
    };
    stream_request(invocation, sequence_number, time, finished.into())
}

fn stream_request(invocation: &Invocation, sequence_number: i64, time: SystemTime, payload: Event) -> PublishBuildToolEventStreamRequest {
    let first = sequence_number == 1;
    let stream_id = stream_id(invocation, &payload);
    PublishBuildToolEventStreamRequest {
        ordered_build_event: MessageField::some(OrderedBuildEvent {
            stream_id: MessageField::some(stream_id),
            sequence_number,
            event: MessageField::some(BuildEvent {
                event_time: MessageField::some(Timestamp::from(time)),
                event: Some(payload),
                ..Default::default()
            }),
            ..Default::default()
        }),
        notification_keywords: if first { invocation.keywords.clone() } else { Vec::new() },
        project_id: invocation.project_id.clone(),
        check_preceding_lifecycle_events_present: first && invocation.check_preceding_lifecycle_events,
        ..Default::default()
    }
}

/// The stream an event belongs to: the build's, for the build's own
/// lifecycle; the invocation's, for the rest; from the tool, for the
/// stream's events.
fn stream_id(invocation: &Invocation, payload: &Event) -> StreamId {
    let (invocation_id, component) = match payload {
        Event::BuildEnqueued(_) | Event::BuildFinished(_) => (String::new(), BuildComponent::CONTROLLER),
        Event::BazelEvent(_) | Event::ComponentStreamFinished(_) => (invocation.invocation_id.clone(), BuildComponent::TOOL),
        _ => (invocation.invocation_id.clone(), BuildComponent::CONTROLLER),
    };
    StreamId {
        build_id: invocation.build_id.clone(),
        invocation_id,
        component: EnumValue::from(component),
        ..Default::default()
    }
}

/// The sequence number of a stream request.
pub fn sequence_number(request: &PublishBuildToolEventStreamRequest) -> i64 {
    request.ordered_build_event.as_option().map_or(0, |event| event.sequence_number)
}
