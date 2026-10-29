use super::*;
use bazel_bep::events;
use build_rust_connect::google::devtools::build::v1::__buffa::oneof::build_event::Event;
use build_rust_connect::google::devtools::build::v1::build_status::Result as BuildResult;
use build_rust_connect::google::devtools::build::v1::stream_id::BuildComponent;
use build_rust_connect::google::devtools::build::v1::{PublishBuildToolEventStreamRequest, PublishLifecycleEventRequest};
use buffa::Message;
use fake_bes::FakeBes;

const CA: &[u8] = include_bytes!("testdata/ca.pem");
const SERVER_CERTIFICATE: &[u8] = include_bytes!("testdata/server.pem");
const SERVER_KEY: &[u8] = include_bytes!("testdata/server.key");

fn invocation() -> Invocation {
    Invocation {
        build_id: "build".to_string(),
        invocation_id: "invocation".to_string(),
        attempt_number: 1,
        command_start: SystemTime::now(),
        keywords: vec!["command_name=build".to_string(), "protocol_name=BEP".to_string()],
        project_id: "instance".to_string(),
        check_preceding_lifecycle_events: false,
    }
}

fn quick() -> UploadOptions {
    UploadOptions {
        retry_delay: Duration::from_millis(1),
        ..UploadOptions::default()
    }
}

/// Post a short command's events, ending in success, and close.
async fn upload_command(service: &BuildEventService, options: UploadOptions) -> Result<(), TransportError> {
    let upload = service.upload(invocation(), options);
    upload.post(events::build_metadata());
    upload.post(events::workspace_status(&[]));
    let mut last = events::build_finished(0, SystemTime::now());
    last.last_message = true;
    upload.post(last);
    upload.close().await
}

fn lifecycle_kind(request: &PublishLifecycleEventRequest) -> (&'static str, i64, Option<i32>) {
    let ordered = request.build_event.as_option().unwrap();
    let event = ordered.event.as_option().unwrap();
    match event.event.as_ref().unwrap() {
        Event::BuildEnqueued(_) => ("enqueued", ordered.sequence_number, None),
        Event::InvocationAttemptStarted(_) => ("started", ordered.sequence_number, None),
        Event::InvocationAttemptFinished(finished) => ("invocation finished", ordered.sequence_number, Some(finished.invocation_status.result.to_i32())),
        Event::BuildFinished(finished) => ("build finished", ordered.sequence_number, Some(finished.status.result.to_i32())),
        _ => ("other", ordered.sequence_number, None),
    }
}

/// The build events a stream carried, decoded, or `None` for its end.
fn bazel_events(requests: &[PublishBuildToolEventStreamRequest]) -> Vec<(i64, Option<events::BuildEvent>)> {
    let mut decoded = Vec::new();
    for request in requests {
        let ordered = request.ordered_build_event.as_option().unwrap();
        let event = match ordered.event.as_option().unwrap().event.as_ref().unwrap() {
            Event::BazelEvent(any) => {
                assert_eq!(any.type_url, BAZEL_EVENT_TYPE);
                Some(events::BuildEvent::decode(&mut &any.value[..]).unwrap())
            }
            Event::ComponentStreamFinished(_) => None,
            other => panic!("unexpected stream event {other:?}"),
        };
        decoded.push((ordered.sequence_number, event));
    }
    decoded
}

#[tokio::test]
async fn uploads_lifecycle_and_stream_as_bazel_does() {
    let fake = FakeBes::new();
    let address = fake.serve().await.unwrap();
    let headers = vec![("x-team".to_string(), "a".to_string()), ("x-team".to_string(), "b".to_string())];
    let service = BuildEventService::new(&format!("grpc://{address}"), None, &headers).unwrap();
    upload_command(&service, quick()).await.unwrap();

    let received = fake.received();
    let succeeded = Some(BuildResult::COMMAND_SUCCEEDED as i32);
    let lifecycle: Vec<_> = received.lifecycle.iter().map(lifecycle_kind).collect();
    assert_eq!(lifecycle, vec![("enqueued", 1, None), ("started", 1, None), ("invocation finished", 2, succeeded), ("build finished", 2, succeeded)]);
    let enqueued = received.lifecycle[0].build_event.as_option().unwrap();
    assert_eq!(enqueued.stream_id.component.to_i32(), BuildComponent::CONTROLLER as i32);
    assert_eq!(enqueued.stream_id.invocation_id, "");
    assert_eq!(received.lifecycle[0].project_id, "instance");
    assert!(received.lifecycle[2].notification_keywords.is_empty());

    let streamed = bazel_events(&received.stream);
    let numbers: Vec<i64> = streamed.iter().map(|(number, _)| *number).collect();
    assert_eq!(numbers, vec![1, 2, 3, 4]);
    assert!(streamed[3].1.is_none());
    assert!(streamed[2].1.as_ref().unwrap().last_message);
    let first = &received.stream[0];
    assert_eq!(first.notification_keywords, vec!["command_name=build", "protocol_name=BEP"]);
    assert!(received.stream[1].notification_keywords.is_empty());
    let stream_id = &first.ordered_build_event.as_option().unwrap().stream_id;
    assert_eq!((stream_id.build_id.as_str(), stream_id.invocation_id.as_str()), ("build", "invocation"));
    assert_eq!(stream_id.component.to_i32(), BuildComponent::TOOL as i32);
    assert!(received.headers.iter().all(|headers| headers.get_all("x-team").iter().eq(["b"])));
}

#[tokio::test]
async fn sends_unacknowledged_events_again_on_a_new_stream() {
    let fake = FakeBes::failing_first_stream_after(1);
    let address = fake.serve().await.unwrap();
    let service = BuildEventService::new(&format!("grpc://{address}"), None, &[]).unwrap();
    upload_command(&service, quick()).await.unwrap();

    let received = fake.received();
    assert_eq!(received.streams, 2);
    let numbers: Vec<i64> = bazel_events(&received.stream).iter().map(|(number, _)| *number).collect();
    assert_eq!(numbers, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn skips_lifecycle_events_when_asked() {
    let fake = FakeBes::new();
    let address = fake.serve().await.unwrap();
    let service = BuildEventService::new(&format!("grpc://{address}"), None, &[]).unwrap();
    let options = UploadOptions {
        lifecycle_events: false,
        ..quick()
    };
    upload_command(&service, options).await.unwrap();
    let received = fake.received();
    assert!(received.lifecycle.is_empty());
    assert_eq!(received.stream.len(), 4);
}

#[tokio::test]
async fn reports_a_service_it_cannot_reach() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = unused.local_addr().unwrap();
    drop(unused);
    let service = BuildEventService::new(&format!("grpc://{address}"), None, &[]).unwrap();
    let options = UploadOptions { max_retries: 2, ..quick() };
    let error = upload_command(&service, options).await.unwrap_err();
    let TransportError::Upload { message, transient } = error else {
        panic!("expected an upload error, got {error:?}");
    };
    assert!(transient);
    assert!(message.starts_with("All 2 retry attempts failed. UNAVAILABLE"), "{message}");
}

#[tokio::test]
async fn uploads_over_tls_trusting_the_given_certificate() {
    let fake = FakeBes::new();
    let address = fake.serve_tls(SERVER_CERTIFICATE, SERVER_KEY).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, CA).unwrap();
    let url = format!("grpcs://localhost:{}", address.port());
    let service = BuildEventService::new(&url, Some(&ca), &[]).unwrap();
    upload_command(&service, quick()).await.unwrap();
    assert_eq!(fake.received().stream.len(), 4);

    let untrusting = BuildEventService::new(&url, None, &[]).unwrap();
    let options = UploadOptions { max_retries: 0, ..quick() };
    assert!(upload_command(&untrusting, options).await.is_err());
}

#[tokio::test]
async fn times_out_waiting_for_the_upload() {
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = unused.local_addr().unwrap();
    drop(unused);
    let service = BuildEventService::new(&format!("grpc://{address}"), None, &[]).unwrap();
    let options = UploadOptions {
        timeout: Some(Duration::from_millis(50)),
        retry_delay: Duration::from_secs(10),
        ..UploadOptions::default()
    };
    let error = upload_command(&service, options).await.unwrap_err();
    assert!(matches!(error, TransportError::Timeout), "{error:?}");
}

#[test]
fn rejects_bad_headers() {
    let headers = vec![("bad header".to_string(), "x".to_string())];
    assert!(matches!(BuildEventService::new("grpc://localhost:1", None, &headers), Err(BesError::Header { .. })));
}
