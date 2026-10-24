//! A client of a remote execution service: its CAS, action cache and
//! executor behind the same traits as this process's own, so builds can
//! run either here or remotely.

mod action_cache;
mod cas;
mod executor;
mod tiered;

pub use action_cache::RemoteActionCache;
pub use cas::RemoteCas;
pub use executor::RemoteExecutor;
pub use tiered::{TieredActionCache, TieredCas};

use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};

/// Where a remote execution service is: `grpc://host:port`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub uri: http::Uri,
    pub tls: bool,
    /// The REAPI instance name requests name.
    pub instance_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("invalid remote URL '{url}': {reason}")]
    Invalid { url: String, reason: String },
}

impl Endpoint {
    /// Parse a URL as Bazel's `--remote_executor` takes it. A URL without a
    /// scheme means TLS, as in Bazel, which bb doesn't support yet.
    pub fn parse(url: &str, instance_name: &str) -> Result<Endpoint, EndpointError> {
        let invalid = |reason: &str| EndpointError::Invalid {
            url: url.to_string(),
            reason: reason.to_string(),
        };
        let (scheme, rest) = url.split_once("://").unwrap_or(("grpcs", url));
        let (http_scheme, tls) = match scheme {
            "grpc" | "http" => ("http", false),
            "grpcs" | "https" => return Err(invalid("bb doesn't support TLS yet; use grpc://")),
            _ => return Err(invalid("the scheme must be grpc://")),
        };
        let uri: http::Uri = format!("{http_scheme}://{rest}").parse().map_err(|_| invalid("not a URL"))?;
        Ok(Endpoint {
            uri,
            tls,
            instance_name: instance_name.to_string(),
        })
    }

    /// A client of the service, speaking gRPC.
    pub(crate) fn client(&self) -> (HttpClient, ClientConfig) {
        let http = HttpClient::plaintext_http2_only();
        (http, ClientConfig::new(self.uri.clone()).with_protocol(Protocol::Grpc))
    }

    /// A resource name of the instance, for ByteStream.
    pub(crate) fn resource(&self, path: &str) -> String {
        match self.instance_name.is_empty() {
            true => path.to_string(),
            false => format!("{}/{path}", self.instance_name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoints() {
        let plain = Endpoint::parse("grpc://localhost:8980", "").unwrap();
        assert!(!plain.tls);
        assert_eq!(plain.uri.to_string(), "http://localhost:8980/");
        let named = Endpoint::parse("grpc://remote.example.com", "main").unwrap();
        assert_eq!(named.resource("blobs/x/1"), "main/blobs/x/1");
        assert!(Endpoint::parse("remote.example.com", "").is_err());
        assert!(Endpoint::parse("ftp://x", "").is_err());
    }
}
