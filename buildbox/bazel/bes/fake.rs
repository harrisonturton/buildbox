//! A Build Event Service for tests: it records every request and
//! acknowledges each stream request in turn, and can be told to fail a
//! stream part way, to exercise retries.

use build_rust_connect::google::devtools::build::v1::{
    PublishBuildEvent, PublishBuildEventServer, PublishBuildToolEventStreamRequest, PublishBuildToolEventStreamResponse,
    PublishLifecycleEventRequest,
};
use buffa_types::google::protobuf::Empty;
use connectrpc::{ConnectError, ConnectRpcService, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use futures::StreamExt;
use http::HeaderMap;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// What the service has been sent.
#[derive(Clone, Debug, Default)]
pub struct Received {
    pub lifecycle: Vec<PublishLifecycleEventRequest>,
    /// Every stream request, from every stream, in the order they came.
    pub stream: Vec<PublishBuildToolEventStreamRequest>,
    /// How many streams were opened.
    pub streams: usize,
    /// The headers of every request.
    pub headers: Vec<HeaderMap>,
}

/// A recording Build Event Service.
#[derive(Clone, Default)]
pub struct FakeBes {
    received: Arc<Mutex<Received>>,
    /// Fail the first stream with `unavailable` after acknowledging this
    /// many requests.
    fail_first_stream_after: Option<usize>,
}

impl FakeBes {
    pub fn new() -> FakeBes {
        FakeBes::default()
    }

    /// A service whose first stream fails after `acks` acknowledgements.
    pub fn failing_first_stream_after(acks: usize) -> FakeBes {
        FakeBes {
            fail_first_stream_after: Some(acks),
            ..FakeBes::default()
        }
    }

    /// What the service has been sent so far.
    pub fn received(&self) -> Received {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Received> {
        self.received.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn service(&self) -> ConnectRpcService<PublishBuildEventServer<FakeBes>> {
        ConnectRpcService::new(PublishBuildEventServer::new(self.clone()))
    }

    /// Serve in plaintext on a port of localhost, until the runtime stops.
    pub async fn serve(&self) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(accept(listener, self.service(), None));
        Ok(address)
    }

    /// Serve over TLS on a port of localhost with the PEM certificate chain
    /// and key given, until the runtime stops.
    pub async fn serve_tls(&self, certificates: &[u8], key: &[u8]) -> std::io::Result<SocketAddr> {
        let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(certificates).collect::<Result<_, _>>().map_err(std::io::Error::other)?;
        let key = PrivateKeyDer::from_pem_slice(key).map_err(std::io::Error::other)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(std::io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .map_err(std::io::Error::other)?;
        config.alpn_protocols = vec![b"h2".to_vec()];
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        tokio::spawn(accept(listener, self.service(), Some(acceptor)));
        Ok(address)
    }

    fn record_headers(&self, context: &RequestContext) {
        self.lock().headers.push(context.headers().clone());
    }
}

type Service = ConnectRpcService<PublishBuildEventServer<FakeBes>>;

async fn accept(listener: TcpListener, service: Service, tls: Option<tokio_rustls::TlsAcceptor>) {
    while let Ok((tcp, _)) = listener.accept().await {
        let service = service.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                Some(tls) => match tls.accept(tcp).await {
                    Ok(stream) => serve_connection(TokioIo::new(stream), service).await,
                    Err(_) => {}
                },
                None => serve_connection(TokioIo::new(tcp), service).await,
            }
        });
    }
}

async fn serve_connection<I>(io: I, service: Service)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let handler = hyper::service::service_fn(move |request| tower::ServiceExt::oneshot(service.clone(), request));
    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new()).serve_connection(io, handler).await;
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl PublishBuildEvent for FakeBes {
    async fn publish_lifecycle_event(&self, context: RequestContext, request: ServiceRequest<'_, PublishLifecycleEventRequest>) -> ServiceResult<Empty> {
        self.record_headers(&context);
        self.lock().lifecycle.push(request.to_owned_message());
        Response::ok(Empty::default())
    }

    async fn publish_build_tool_event_stream(
        &self,
        context: RequestContext,
        mut requests: InboundStream<PublishBuildToolEventStreamRequest>,
    ) -> ServiceResult<ServiceStream<PublishBuildToolEventStreamResponse>> {
        self.record_headers(&context);
        let first = {
            let mut received = self.lock();
            received.streams += 1;
            received.streams == 1
        };
        let fail_after = self.fail_first_stream_after.filter(|_| first);
        let (acks, sent) = mpsc::unbounded_channel();
        let fake = self.clone();
        tokio::spawn(async move {
            let mut acked = 0;
            while let Some(request) = requests.next().await {
                let Ok(request) = request else {
                    return;
                };
                if fail_after == Some(acked) {
                    let _ = acks.send(Err(ConnectError::unavailable("the fake service failed the stream")));
                    return;
                }
                let request = request.to_owned_message();
                let ack = PublishBuildToolEventStreamResponse {
                    stream_id: request.ordered_build_event.as_option().map(|event| event.stream_id.clone()).unwrap_or_default(),
                    sequence_number: request.ordered_build_event.as_option().map_or(0, |event| event.sequence_number),
                    ..Default::default()
                };
                fake.lock().stream.push(request);
                acked += 1;
                if acks.send(Ok(ack)).is_err() {
                    return;
                }
            }
        });
        let stream: ServiceStream<PublishBuildToolEventStreamResponse> = Box::pin(UnboundedReceiverStream::new(sent));
        Response::ok(stream)
    }
}
