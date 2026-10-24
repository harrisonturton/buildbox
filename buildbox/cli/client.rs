//! Connecting to a running server's admin API.

use admin_rust_connect::buildbox::admin::v1::{
    AdminClient, GetStatusRequest, GetStatusResponse, ListActionsRequest, ListActionsResponse,
};
use clap::Args;
use config::{Environment, Settings};
use connectrpc::client::{ClientConfig, HttpClient};
use connectrpc::ConnectError;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

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
    pub fn connect(&self) -> Result<Admin, Box<dyn Error>> {
        let url = match &self.server {
            Some(url) => url.clone(),
            None => self.configured_url()?,
        };

        let uri = match url.parse() {
            Ok(uri) => uri,
            Err(error) => return Err(format!("invalid server URL {url:?}: {error}").into()),
        };
        let client = AdminClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
        Ok(Admin { client, url })
    }

    fn configured_url(&self) -> Result<String, Box<dyn Error>> {
        let env = Environment::from_process();
        let config = config::load(self.config.as_deref(), Settings::default(), &env)?;
        Ok(url_for(config.listen))
    }
}

/// A client for a server's admin API. Connections are made as calls need
/// them, so an unreachable server is reported by the first call.
pub struct Admin {
    client: AdminClient<HttpClient>,
    url: String,
}

impl Admin {
    pub async fn get_status(&self) -> Result<GetStatusResponse, Box<dyn Error>> {
        let response = self.client.get_status(GetStatusRequest::default()).await;
        match response {
            Ok(response) => Ok(response.into_owned()),
            Err(error) => Err(self.call_error(error)),
        }
    }

    pub async fn list_actions(&self) -> Result<ListActionsResponse, Box<dyn Error>> {
        let response = self.client.list_actions(ListActionsRequest::default()).await;
        match response {
            Ok(response) => Ok(response.into_owned()),
            Err(error) => Err(self.call_error(error)),
        }
    }

    fn call_error(&self, error: ConnectError) -> Box<dyn Error> {
        format!("request to {} failed: {error}", self.url).into()
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
