use std::sync::Mutex;
use std::time::Duration;

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer as _;

use crate::export::{self, Finished};
use crate::layer::{self, Kind, Value};
use crate::otlp_json::OtlpJsonFile;
use crate::{set_attribute, set_error, SpanContext};

/// The layer's queue is the process's, so tests that record take turns.
static RECORDING: Mutex<()> = Mutex::new(());

/// What `work` records, with recording turned on.
fn recorded(work: impl FnOnce()) -> Vec<Finished> {
    let _turn = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    export::record_for_tests();
    export::take_all();
    let subscriber = tracing_subscriber::registry().with(layer::Layer.with_filter(LevelFilter::INFO));
    tracing::subscriber::with_default(subscriber, work);
    export::take_all()
}

#[test]
fn parses_and_writes_traceparent() {
    let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let context = SpanContext::from_traceparent(header).expect("a valid header");
    assert_eq!(context.trace.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(context.span.to_string(), "00f067aa0ba902b7");
    assert_eq!(context.traceparent(), header);
}

#[test]
fn rejects_what_isnt_a_traceparent() {
    for header in [
        "",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
        "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
        "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "00-4bf92f3577b34da6a3ce929d0e0e47-00f067aa0ba902b7-01",
    ] {
        assert_eq!(SpanContext::from_traceparent(header), None, "{header}");
    }
    assert!(
        SpanContext::from_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-later").is_some(),
        "a later version may add parts"
    );
}

#[test]
fn nests_spans_in_one_trace() {
    let spans = recorded(|| {
        let outer = tracing::info_span!("outer");
        let _outer = outer.enter();
        tracing::info_span!("inner", op = "get", n = 3).in_scope(|| {});
    });
    let [inner, outer] = &spans[..] else {
        panic!("two spans, inner first: {}", spans.len());
    };
    assert_eq!(outer.name, "outer");
    assert_eq!(outer.parent, None);
    assert_eq!(inner.name, "inner");
    assert_eq!(inner.context.trace, outer.context.trace);
    assert_eq!(inner.parent, Some(outer.context.span));
    assert_eq!(inner.attributes, vec![("op", Value::Str("get".into())), ("n", Value::Int(3))]);
    assert!(inner.started >= outer.started && inner.ended <= outer.ended);
}

#[test]
fn continues_a_trace_from_traceparent() {
    let header = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let remote = SpanContext::from_traceparent(header).expect("a valid header");
    let spans = recorded(|| {
        tracing::info_span!(
            "rpc",
            span.name = "build.bazel.remote.execution.v2.Execution/Execute",
            span.kind = "server",
            traceparent = header,
        )
        .in_scope(|| {});
    });
    let [rpc] = &spans[..] else {
        panic!("one span: {}", spans.len());
    };
    assert_eq!(rpc.name, "build.bazel.remote.execution.v2.Execution/Execute");
    assert_eq!(rpc.kind, Kind::Server);
    assert_eq!(rpc.context.trace, remote.trace);
    assert_eq!(rpc.parent, Some(remote.span));
    assert!(rpc.attributes.is_empty(), "{:?}", rpc.attributes);
}

#[test]
fn starts_a_trace_without_a_valid_traceparent() {
    let spans = recorded(|| {
        tracing::info_span!("rpc", traceparent = "").in_scope(|| {});
    });
    let [rpc] = &spans[..] else {
        panic!("one span: {}", spans.len());
    };
    assert_eq!(rpc.parent, None);
}

#[test]
fn links_to_the_traces_that_caused_a_span() {
    let first = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let second = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let spans = recorded(|| {
        let span = tracing::info_span!("work", link = first);
        span.record("link", second);
        span.record("link", "not a traceparent");
        span.in_scope(|| {});
    });
    let [work] = &spans[..] else {
        panic!("one span: {}", spans.len());
    };
    let links: Vec<_> = work.links.iter().map(|l| l.traceparent()).collect();
    assert_eq!(links, [first, second]);
    assert_eq!(work.parent, None, "a link isn't a parent");
}

#[test]
fn marks_failed_spans() {
    let spans = recorded(|| {
        tracing::info_span!("given", error = tracing::field::Empty).in_scope(|| {
            tracing::Span::current().record("error", "no such thing");
        });
        tracing::info_span!("logged").in_scope(|| tracing::error!("it broke"));
        tracing::info_span!("set").in_scope(|| set_error("action failed".to_string()));
        tracing::info_span!("fine").in_scope(|| tracing::warn!("not an error"));
    });
    let errors: Vec<_> = spans.iter().map(|s| s.error.as_deref()).collect();
    assert_eq!(errors, [Some("no such thing"), Some("it broke"), Some("action failed"), None]);
}

#[test]
fn leaves_out_debug_spans_but_keeps_their_children_in_the_trace() {
    let spans = recorded(|| {
        tracing::info_span!("outer").in_scope(|| {
            tracing::debug_span!("hidden").in_scope(|| {
                tracing::info_span!("inner").in_scope(|| {});
            });
        });
    });
    let [inner, outer] = &spans[..] else {
        panic!("two spans: {}", spans.len());
    };
    assert_eq!(inner.parent, Some(outer.context.span));
}

#[test]
fn starts_no_trace_from_a_nested_span() {
    let spans = recorded(|| {
        tracing::info_span!("store", span.nested = true).in_scope(|| {
            tracing::info_span!("inner").in_scope(|| {});
        });
        tracing::info_span!("rpc").in_scope(|| {
            tracing::info_span!("store", span.nested = true).in_scope(|| {});
        });
    });
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["store", "rpc"]);
    assert_eq!(spans[0].parent, Some(spans[1].context.span));
}

#[test]
fn ends_a_span_when_it_was_last_exited() {
    let spans = recorded(|| {
        let span = tracing::info_span!("work");
        span.in_scope(|| {});
        let exited = std::time::SystemTime::now();
        std::thread::sleep(Duration::from_millis(50));
        drop(span);
        assert!(exited.elapsed().unwrap() >= Duration::from_millis(50));
    });
    let [work] = &spans[..] else {
        panic!("one span: {}", spans.len());
    };
    let duration = work.ended.duration_since(work.started).unwrap();
    assert!(duration < Duration::from_millis(50), "{duration:?}");
}

#[test]
fn adds_undeclared_attributes_to_the_current_span() {
    let spans = recorded(|| {
        tracing::info_span!("rpc").in_scope(|| {
            set_attribute("blobs", 3usize);
            set_attribute("cached", true);
        });
        set_attribute("outside", "any span");
    });
    let [rpc] = &spans[..] else {
        panic!("one span: {}", spans.len());
    };
    assert_eq!(rpc.attributes, vec![("blobs", Value::Int(3)), ("cached", Value::Bool(true))]);
}

#[test]
fn writes_one_line_of_otlp_json_per_batch() {
    let spans = recorded(|| {
        tracing::info_span!("rpc", span.kind = "server", ok = true).in_scope(|| {
            tracing::info_span!("inner", error = "broken").in_scope(|| {});
        });
    });
    let [inner, rpc] = &spans[..] else {
        panic!("two spans: {}", spans.len());
    };
    let (rpc_context, inner_context) = (rpc.context, inner.context);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("traces/spans.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{}\n").unwrap();

    let mut file = OtlpJsonFile::open(&path, "buildbox", "1.2.3").unwrap();
    file.write(spans).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<serde_json::Value> = content.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 2, "appends to what's there");
    let resource_spans = &lines[1]["resourceSpans"][0];
    assert_eq!(resource_spans["resource"]["attributes"][0]["key"], "service.name");
    assert_eq!(resource_spans["resource"]["attributes"][1]["value"]["stringValue"], "1.2.3");
    assert_eq!(resource_spans["scopeSpans"][0]["scope"]["name"], "buildbox");

    let [inner, rpc] = &resource_spans["scopeSpans"][0]["spans"].as_array().unwrap()[..] else {
        panic!("two spans");
    };
    assert_eq!(rpc["traceId"], rpc_context.trace.to_string());
    assert_eq!(rpc["spanId"], rpc_context.span.to_string());
    assert!(rpc.get("parentSpanId").is_none());
    assert_eq!(rpc["kind"], 2);
    assert_eq!(rpc["attributes"][0]["key"], "ok");
    assert_eq!(rpc["attributes"][0]["value"]["boolValue"], true);
    assert_eq!(rpc["status"]["code"], 0);
    let start: u128 = rpc["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: u128 = rpc["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(end >= start);

    assert_eq!(inner["spanId"], inner_context.span.to_string());
    assert_eq!(inner["parentSpanId"], rpc_context.span.to_string());
    assert_eq!(inner["status"]["code"], 2);
    assert_eq!(inner["status"]["message"], "broken");
}
