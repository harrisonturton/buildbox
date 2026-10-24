//! Connecting to a running server's admin API.

use crate::blobs::Blobs;
use admin_rust_connect::buildbox::admin::v1::{
    AdminClient, Artifact, DeleteArtifactRequest, GetStatusRequest, GetStatusResponse, ImportContainerImageRequest,
    ListActionsRequest, ListActionsResponse, ListArtifactsRequest,
};
use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, CancelRequest, GetServerRequest, GetServerResponse};
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
        let (uri, url) = self.server_uri()?;
        let client = AdminClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
        Ok(Admin { client, url })
    }

    /// Connect to the server's Bazel service, to inspect and control it.
    pub fn connect_server(&self) -> Result<Control, Box<dyn Error>> {
        let (uri, url) = self.server_uri()?;
        let client = BazelClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
        Ok(Control { client, url })
    }

    /// Connect to the server's CAS, to upload blobs.
    pub fn connect_blobs(&self) -> Result<Blobs, Box<dyn Error>> {
        let (uri, url) = self.server_uri()?;
        Ok(Blobs::new(uri, url))
    }

    fn server_uri(&self) -> Result<(http::Uri, String), Box<dyn Error>> {
        let url = match &self.server {
            Some(url) => url.clone(),
            None => self.configured_url()?,
        };

        match url.parse() {
            Ok(uri) => Ok((uri, url)),
            Err(error) => Err(format!("invalid server URL {url:?}: {error}").into()),
        }
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

    pub async fn import_container_image(&self, name: &str, manifest: &cas::Digest) -> Result<Artifact, Box<dyn Error>> {
        let request = ImportContainerImageRequest {
            name: name.to_string(),
            manifest_digest: manifest.to_string(),
            ..Default::default()
        };
        let response = self.client.import_container_image(request).await;
        let response = response.map_err(|error| self.call_error(error))?.into_owned();
        Ok(response.artifact.into_option().unwrap_or_default())
    }

    pub async fn list_artifacts(&self) -> Result<Vec<Artifact>, Box<dyn Error>> {
        let response = self.client.list_artifacts(ListArtifactsRequest::default()).await;
        let response = response.map_err(|error| self.call_error(error))?;
        Ok(response.into_owned().artifacts)
    }

    pub async fn delete_artifact(&self, name: &str) -> Result<(), Box<dyn Error>> {
        let request = DeleteArtifactRequest {
            name: name.to_string(),
            ..Default::default()
        };
        let response = self.client.delete_artifact(request).await;
        response.map_err(|error| self.call_error(error))?;
        Ok(())
    }

    fn call_error(&self, error: ConnectError) -> Box<dyn Error> {
        format!("request to {} failed: {error}", self.url).into()
    }
}

/// A client for a server's Bazel service, as far as inspecting and
/// controlling the server goes.
pub struct Control {
    client: BazelClient<HttpClient>,
    url: String,
}

impl Control {
    pub async fn get_server(&self) -> Result<GetServerResponse, Box<dyn Error>> {
        let response = self.client.get_server(GetServerRequest::default()).await;
        match response {
            Ok(response) => Ok(response.into_owned()),
            Err(error) => Err(format!("request to {} failed: {error}", self.url).into()),
        }
    }

    /// Cancel a command, returning whether it was running.
    pub async fn cancel(&self, invocation_id: &str) -> Result<bool, Box<dyn Error>> {
        let request = CancelRequest {
            invocation_id: invocation_id.to_string(),
            ..Default::default()
        };
        let response = self.client.cancel(request).await;
        match response {
            Ok(response) => Ok(response.into_owned().cancelled),
            Err(error) => Err(format!("request to {} failed: {error}", self.url).into()),
        }
    }
}

/// The URL to reach a server listening on `listen`. A server listening on
/// every interface is reached through loopback.
pub fn url_for(listen: SocketAddr) -> String {
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
