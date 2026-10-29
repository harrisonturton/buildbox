//! Clients of gRPC services: where a service is, as Bazel's flags name it,
//! and channels that generated clients send requests over, in plaintext
//! HTTP/2 or over TLS.

mod tls;

pub use tls::TlsError;

use connectrpc::client::{BoxFuture, ClientBody, ClientConfig, ClientTransport, Http2Connection, HttpClient, SharedHttp2Connection};
use connectrpc::{ConnectError, Protocol};
use http::{Request, Response, Uri};
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// Where a gRPC service is: `grpc://host:port` in plaintext, or
/// `grpcs://host:port` over TLS. A URL without a scheme means TLS, as in
/// Bazel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The service's URI, as `http://` or `https://`.
    pub uri: Uri,
    pub tls: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error("invalid URL '{url}': {reason}")]
    Invalid { url: String, reason: &'static str },
}

impl Target {
    /// Parse a URL as Bazel's `--remote_executor` and `--bes_backend` take
    /// it.
    pub fn parse(url: &str) -> Result<Target, TargetError> {
        let invalid = |reason| TargetError::Invalid { url: url.to_string(), reason };
        let (scheme, rest) = url.split_once("://").unwrap_or(("grpcs", url));
        let (http_scheme, tls) = match scheme {
            "grpc" | "http" => ("http", false),
            "grpcs" | "https" => ("https", true),
            _ => return Err(invalid("the scheme must be grpc:// or grpcs://")),
        };
        let uri: Uri = format!("{http_scheme}://{rest}").parse().map_err(|_| invalid("not a URL"))?;
        if uri.host().is_none_or(str::is_empty) {
            return Err(invalid("there is no host"));
        }
        Ok(Target { uri, tls })
    }
}

/// A connection to a service, made when the first request is sent and
/// made again if it drops, which generated clients are built on.
#[derive(Clone, Debug)]
pub struct Channel {
    target: Target,
    transport: Transport,
}

#[derive(Clone, Debug)]
enum Transport {
    Plaintext(HttpClient),
    Tls(Arc<TlsTransport>),
}

/// One HTTP/2 connection over TLS, shared by every request.
struct TlsTransport {
    authority: Uri,
    config: Arc<rustls::ClientConfig>,
    connection: OnceLock<SharedHttp2Connection>,
}

impl std::fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsTransport").field("authority", &self.authority).finish()
    }
}

/// How many requests may wait for the TLS connection at once.
const TLS_QUEUE: usize = 1024;

impl Channel {
    /// A channel to `target`. Over TLS it trusts Mozilla's root
    /// certificates, as Bazel trusts its JDK's, and those in the PEM file
    /// `certificates`, as Bazel's `--tls_certificate` names.
    pub fn new(target: &Target, certificates: Option<&Path>) -> Result<Channel, TlsError> {
        let transport = match target.tls {
            false => Transport::Plaintext(HttpClient::plaintext_http2_only()),
            true => Transport::Tls(Arc::new(TlsTransport {
                authority: target.uri.clone(),
                config: tls::client_config(certificates)?,
                connection: OnceLock::new(),
            })),
        };
        Ok(Channel { target: target.clone(), transport })
    }

    /// Where the channel connects.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// A configuration for generated clients on this channel, speaking gRPC.
    pub fn config(&self) -> ClientConfig {
        ClientConfig::new(self.target.uri.clone()).with_protocol(Protocol::Grpc)
    }
}

impl TlsTransport {
    /// The connection, made the first time it's needed, which must be on a
    /// Tokio runtime.
    fn connection(&self) -> SharedHttp2Connection {
        let connection = self.connection.get_or_init(|| {
            let connector = tls::Connector::new(self.config.clone());
            Http2Connection::builder().lazy_with_connector(connector, self.authority.clone()).shared(TLS_QUEUE)
        });
        connection.clone()
    }
}

impl ClientTransport for Channel {
    type ResponseBody = hyper::body::Incoming;
    type Error = ConnectError;

    fn send(&self, request: Request<ClientBody>) -> BoxFuture<'static, Result<Response<Self::ResponseBody>, Self::Error>> {
        match &self.transport {
            Transport::Plaintext(client) => client.send(request),
            Transport::Tls(tls) => tls.connection().send(request),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let plain = Target::parse("grpc://localhost:8980").unwrap();
        assert!(!plain.tls);
        assert_eq!(plain.uri.to_string(), "http://localhost:8980/");
        let tls = Target::parse("grpcs://remote.example.com").unwrap();
        assert!(tls.tls);
        assert_eq!(tls.uri.to_string(), "https://remote.example.com/");
        let bare = Target::parse("remote.example.com:443").unwrap();
        assert!(bare.tls);
        assert_eq!(bare.uri.to_string(), "https://remote.example.com:443/");
        assert!(Target::parse("ftp://x").is_err());
        assert!(Target::parse("grpc://").is_err());
    }

    #[test]
    fn makes_channels_without_a_runtime() {
        let target = Target::parse("grpcs://remote.example.com").unwrap();
        let channel = Channel::new(&target, None).unwrap();
        assert_eq!(channel.config().base_uri(), &target.uri);
    }
}
