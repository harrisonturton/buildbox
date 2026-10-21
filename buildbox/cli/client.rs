//! Connecting to a running server's admin API.

use admin_proto::buildbox::admin::v1::admin_client::AdminClient;
use clap::Args;
use config::{Environment, Settings};
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use tonic::transport::{Channel, Endpoint};

#[derive(Args, Debug)]
pub struct ClientArgs {
    /// The server to connect to, e.g. http://[::1]:50051. Defaults to the
    /// listen address in the configuration.
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// The config file to read the server's address from.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

impl ClientArgs {
    pub async fn connect(&self) -> Result<AdminClient<Channel>, Box<dyn Error>> {
        let url = match &self.server {
            Some(url) => url.clone(),
            None => self.configured_url()?,
        };

        let endpoint = Endpoint::from_shared(url.clone())?;
        match AdminClient::connect(endpoint).await {
            Ok(client) => Ok(client),
            Err(error) => Err(format!("failed to connect to {url}: {error}").into()),
        }
    }

    fn configured_url(&self) -> Result<String, Box<dyn Error>> {
        let env = Environment::from_process();
        let config = config::load(self.config.as_deref(), Settings::default(), &env)?;
        Ok(url_for(config.listen))
    }
}

/// The URL to reach a server listening on `listen`. A server listening on
/// every interface is reached through loopback.
fn url_for(listen: SocketAddr) -> String {
    let ip = listen.ip();
    let ip = if !ip.is_unspecified() {
        ip
    } else if ip.is_ipv4() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_for_uses_loopback_for_unspecified_addresses() {
        assert_eq!(url_for("[::1]:50051".parse().unwrap()), "http://[::1]:50051");
        assert_eq!(url_for("192.168.1.2:80".parse().unwrap()), "http://192.168.1.2:80");
        assert_eq!(url_for("0.0.0.0:80".parse().unwrap()), "http://127.0.0.1:80");
        assert_eq!(url_for("[::]:80".parse().unwrap()), "http://[::1]:80");
    }
}
