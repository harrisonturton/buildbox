use internal::proto::remote_execution::{
    digest_function, ActionCacheUpdateCapabilities, CacheCapabilities, Capabilities,
    ExecutionCapabilities, GetCapabilitiesRequest, ServerCapabilities,
};
use semver_proto::build::bazel::semver::SemVer;
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct CapabilitiesService {}

#[async_trait::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _req: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        tracing::info!("Capabilities::get_capabilities");
        let cap = ServerCapabilities {
            cache_capabilities: Some(CacheCapabilities {
                digest_function: vec![digest_function::Value::Sha256.into()],
                action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                ..Default::default()
            }),
            execution_capabilities: Some(ExecutionCapabilities {
                digest_function: digest_function::Value::Sha256.into(),
                exec_enabled: true,
                ..Default::default()
            }),
            low_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            high_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(Response::new(cap))
    }
}
