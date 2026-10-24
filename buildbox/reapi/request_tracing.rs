//! A span for every request the server handles.
//!
//! [`RequestTracing`] intercepts every call the RPC server dispatches, so
//! every service is traced the same way. Each request's span records which
//! RPC it was and what Bazel said about it in its `RequestMetadata`: the
//! action it is for, and the invocation it is part of. Requests are grouped
//! into one trace per invocation (see [`crate::invocations`]), unless the
//! client sent its own trace context, which takes precedence.

use crate::invocations::{InvocationInfo, InvocationRequest, Invocations};
use buffa::Message;
use connectrpc::interceptor::{
    async_trait, Interceptor, Next, NextStream, PayloadStream, StreamRequest, StreamResponse, UnaryRequest,
    UnaryResponse,
};
use connectrpc::{ConnectError, ErrorCode, RequestContext};
use data_encoding::BASE64_NOPAD;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::RequestMetadata;
use std::sync::Arc;
use tracing::field::Empty;
use tracing::{Instrument, Span};

/// The header REAPI clients send `RequestMetadata` in, as a base64-encoded
/// binary proto.
const REQUEST_METADATA_HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

/// Runs each call in its own span. A streaming call's span covers its
/// handler, which for every service here includes reading the requests;
/// sending the responses gets spans of its own where it's done separately.
pub struct RequestTracing {
    invocations: Arc<Invocations>,
}

impl RequestTracing {
    pub fn new(invocations: Arc<Invocations>) -> RequestTracing {
        RequestTracing { invocations }
    }

    /// The span for a call, and the invocation request it's part of, which
    /// must be held until the call is finished.
    fn start(&self, ctx: &RequestContext) -> (Span, Option<InvocationRequest>) {
        let metadata = request_metadata(ctx.headers());
        let invocation = self.invocations.begin(&invocation_info(&metadata));
        let span = request_span(ctx, &metadata, invocation.as_ref());
        (span, invocation)
    }
}

#[async_trait]
impl Interceptor for RequestTracing {
    async fn intercept_unary(&self, request: UnaryRequest, next: Next<'_>) -> Result<UnaryResponse, ConnectError> {
        let (span, invocation) = self.start(&request.ctx);
        let traced = async {
            let response = next.run(request).await;
            record_result(&response);
            // Only now is the request finished.
            drop(invocation);
            response
        };
        traced.instrument(span).await
    }

    async fn intercept_streaming(
        &self,
        request: StreamRequest,
        inbound: PayloadStream,
        next: NextStream<'_>,
    ) -> Result<StreamResponse, ConnectError> {
        let (span, invocation) = self.start(&request.ctx);
        let traced = async {
            let response = next.run(request, inbound).await;
            record_result(&response);
            drop(invocation);
            response
        };
        traced.instrument(span).await
    }
}

fn request_span(ctx: &RequestContext, metadata: &RequestMetadata, invocation: Option<&InvocationRequest>) -> Span {
    // Paths are "<service>/<method>", sometimes with a leading slash.
    let path = ctx.path().unwrap_or_default();
    let rpc = path.strip_prefix('/').unwrap_or(path);
    let (service, method) = rpc.split_once('/').unwrap_or((rpc, ""));

    // A trace the client sent takes precedence over the invocation's.
    let traceparent = ctx.headers().get(telemetry::HEADER).and_then(|value| value.to_str().ok());
    let parent = invocation.and_then(|invocation| invocation.span().id());
    tracing::info_span!(
        parent: parent,
        "rpc",
        span.name = %rpc,
        span.kind = "server",
        traceparent = traceparent.unwrap_or(""),
        error = Empty,
        rpc.service = %service,
        rpc.method = %method,
        rpc.code = Empty,
        bazel.invocation_id = non_empty(&metadata.tool_invocation_id),
        bazel.action_id = non_empty(&metadata.action_id),
    )
}

/// Record how the call answered on the current span.
fn record_result<T>(response: &Result<T, ConnectError>) {
    let span = Span::current();
    let Err(error) = response else {
        span.record("rpc.code", "ok");
        return;
    };
    span.record("rpc.code", error.code.as_str());
    if is_server_error(error.code) {
        span.record(telemetry::ERROR, error.message.as_deref().unwrap_or(error.code.as_str()));
    }
}

/// Whether a code means the server failed, rather than that the request was
/// one it couldn't satisfy, such as a lookup of something missing. Only these
/// mark a span as failed, as OpenTelemetry's conventions for RPC servers say.
fn is_server_error(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::Unknown
            | ErrorCode::DeadlineExceeded
            | ErrorCode::Unimplemented
            | ErrorCode::Internal
            | ErrorCode::Unavailable
            | ErrorCode::DataLoss
    )
}

/// The `RequestMetadata` the client sent, or empty metadata if it sent none
/// or it can't be decoded: it's informational, so a bad header shouldn't fail
/// the request.
fn request_metadata(headers: &http::HeaderMap) -> RequestMetadata {
    let Some(value) = headers.get(REQUEST_METADATA_HEADER) else {
        return RequestMetadata::default();
    };
    // Binary gRPC headers are base64, with or without padding.
    let Ok(decoded) = BASE64_NOPAD.decode(strip_padding(value.as_bytes())) else {
        tracing::debug!("ignoring request metadata that isn't base64");
        return RequestMetadata::default();
    };
    RequestMetadata::decode_from_slice(&decoded).unwrap_or_else(|error| {
        tracing::debug!("ignoring malformed request metadata: {error}");
        RequestMetadata::default()
    })
}

fn strip_padding(encoded: &[u8]) -> &[u8] {
    let unpadded_len = encoded.iter().rposition(|&byte| byte != b'=').map_or(0, |last| last + 1);
    &encoded[..unpadded_len]
}

fn invocation_info(metadata: &RequestMetadata) -> InvocationInfo {
    let tool = metadata.tool_details.as_option().cloned().unwrap_or_default();
    InvocationInfo {
        id: metadata.tool_invocation_id.clone(),
        correlated_invocations_id: metadata.correlated_invocations_id.clone(),
        tool_name: tool.tool_name,
        tool_version: tool.tool_version,
    }
}

/// Fields set to `None` aren't recorded, so absent metadata isn't exported as
/// empty attributes.
fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::BASE64;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::ToolDetails;

    fn headers_with_metadata(encoded: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(REQUEST_METADATA_HEADER, encoded.parse().unwrap());
        headers
    }

    fn metadata() -> RequestMetadata {
        let tool = ToolDetails {
            tool_name: "bazel".to_string(),
            tool_version: "8.0.0".to_string(),
            ..Default::default()
        };
        RequestMetadata {
            tool_details: tool.into(),
            action_id: "action".to_string(),
            tool_invocation_id: "invocation".to_string(),
            correlated_invocations_id: "build".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn decodes_padded_and_unpadded_metadata() {
        let encoded = metadata().encode_to_vec();
        let padded = BASE64.encode(&encoded);
        let unpadded = BASE64_NOPAD.encode(&encoded);

        assert_eq!(request_metadata(&headers_with_metadata(&padded)), metadata());
        assert_eq!(request_metadata(&headers_with_metadata(&unpadded)), metadata());
    }

    #[test]
    fn ignores_missing_or_malformed_metadata() {
        assert_eq!(request_metadata(&http::HeaderMap::new()), RequestMetadata::default());
        assert_eq!(request_metadata(&headers_with_metadata("not base64!")), RequestMetadata::default());
        let not_a_proto = BASE64.encode(b"\xff\xff\xff");
        assert_eq!(request_metadata(&headers_with_metadata(&not_a_proto)), RequestMetadata::default());
    }

    #[test]
    fn invocation_info_comes_from_metadata() {
        let info = invocation_info(&metadata());

        assert_eq!(
            info,
            InvocationInfo {
                id: "invocation".to_string(),
                correlated_invocations_id: "build".to_string(),
                tool_name: "bazel".to_string(),
                tool_version: "8.0.0".to_string(),
            }
        );
    }

    #[test]
    fn only_server_failures_are_errors() {
        assert!(is_server_error(ErrorCode::Internal));
        assert!(!is_server_error(ErrorCode::NotFound));
        assert!(!is_server_error(ErrorCode::FailedPrecondition));
    }
}
