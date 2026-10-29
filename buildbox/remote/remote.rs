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

use connectrpc::client::ClientConfig;
use grpcclient::{Channel, Target, TargetError, TlsError};

/// Where a remote execution service is, `grpc://host:port` or
/// `grpcs://host:port`, and the channel requests to it go over.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub uri: http::Uri,
    /// The REAPI instance name requests name.
    pub instance_name: String,
    channel: Channel,
}

/// Why a remote service can't be used.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Tls(#[from] TlsError),
}

impl Endpoint {
    /// Parse a URL as Bazel's `--remote_executor` takes it. A URL without a
    /// scheme means TLS, as in Bazel.
    pub fn parse(url: &str, instance_name: &str) -> Result<Endpoint, EndpointError> {
        let target = Target::parse(url)?;
        let channel = Channel::new(&target, None)?;
        Ok(Endpoint {
            uri: target.uri,
            instance_name: instance_name.to_string(),
            channel,
        })
    }

    /// A client of the service, speaking gRPC.
    pub(crate) fn client(&self) -> (Channel, ClientConfig) {
        (self.channel.clone(), self.channel.config())
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
        assert_eq!(plain.uri.to_string(), "http://localhost:8980/");
        let named = Endpoint::parse("grpc://remote.example.com", "main").unwrap();
        assert_eq!(named.resource("blobs/x/1"), "main/blobs/x/1");
        let tls = Endpoint::parse("remote.example.com", "").unwrap();
        assert_eq!(tls.uri.to_string(), "https://remote.example.com/");
        assert!(Endpoint::parse("ftp://x", "").is_err());
    }
}
