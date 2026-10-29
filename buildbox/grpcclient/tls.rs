//! TLS connections for gRPC: rustls on ring, offering HTTP/2 by ALPN, and
//! trusting Mozilla's roots plus any certificates the user names.

use connectrpc::client::BoxFuture;
use http::Uri;
use hyper_util::rt::TokioIo;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// The port a `grpcs://` URL without one means.
const DEFAULT_PORT: u16 = 443;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("reading TLS certificates from {path}: {source}")]
    Certificates { path: PathBuf, source: rustls::pki_types::pem::Error },

    #[error("no TLS certificate in {path} could be trusted")]
    NoCertificates { path: PathBuf },

    #[error("configuring TLS: {0}")]
    Config(#[from] rustls::Error),
}

/// A client configuration trusting Mozilla's roots and the certificates in
/// the PEM file `certificates`.
pub fn client_config(certificates: Option<&Path>) -> Result<Arc<rustls::ClientConfig>, TlsError> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(path) = certificates {
        add_certificates(&mut roots, path)?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(config))
}

/// Trust the certificates in a PEM file, which must hold at least one that
/// can be.
fn add_certificates(roots: &mut RootCertStore, path: &Path) -> Result<(), TlsError> {
    let read_error = |source| TlsError::Certificates { path: path.to_path_buf(), source };
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path).map_err(read_error)?.collect::<Result<_, _>>().map_err(read_error)?;
    let (added, _ignored) = roots.add_parsable_certificates(certificates);
    if added == 0 {
        return Err(TlsError::NoCertificates { path: path.to_path_buf() });
    }
    Ok(())
}

/// Dials a URI's host and port and makes a TLS connection to it, checking
/// the server's certificate names the host.
#[derive(Clone)]
pub struct Connector {
    config: Arc<rustls::ClientConfig>,
}

impl Connector {
    pub fn new(config: Arc<rustls::ClientConfig>) -> Connector {
        Connector { config }
    }
}

impl tower::Service<Uri> for Connector {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<Self::Response>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        Box::pin(connect(self.config.clone(), uri))
    }
}

async fn connect(config: Arc<rustls::ClientConfig>, uri: Uri) -> io::Result<TokioIo<TlsStream<TcpStream>>> {
    let Some(host) = uri.host() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("no host in {uri}")));
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = uri.port_u16().unwrap_or(DEFAULT_PORT);
    let name = ServerName::try_from(host.clone()).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{host}: {error}")))?;
    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    tcp.set_nodelay(true)?;
    let stream = tokio_rustls::TlsConnector::from(config).connect(name, tcp).await?;
    Ok(TokioIo::new(stream))
}
