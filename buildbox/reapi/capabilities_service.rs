use crate::MAX_BATCH_TOTAL_SIZE_BYTES;
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    digest_function, ActionCacheUpdateCapabilities, CacheCapabilities, Capabilities, ExecutionCapabilities,
    GetCapabilitiesRequest, ServerCapabilities,
};
use remote_execution_rust_connect::build::bazel::semver::SemVer;

#[derive(Default, Debug)]
pub struct CapabilitiesService {}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetCapabilitiesRequest>,
    ) -> ServiceResult<ServerCapabilities> {
        tracing::info!("Capabilities::get_capabilities");
        let cap = ServerCapabilities {
            cache_capabilities: CacheCapabilities {
                digest_function: vec![digest_function::Value::SHA256.into()],
                action_cache_update_capabilities: ActionCacheUpdateCapabilities {
                    update_enabled: true,
                    ..Default::default()
                }
                .into(),
                max_batch_total_size_bytes: MAX_BATCH_TOTAL_SIZE_BYTES as i64,
                ..Default::default()
            }
            .into(),
            execution_capabilities: ExecutionCapabilities {
                digest_function: digest_function::Value::SHA256.into(),
                exec_enabled: true,
                ..Default::default()
            }
            .into(),
            low_api_version: api_version().into(),
            high_api_version: api_version().into(),
            ..Default::default()
        };
        Response::ok(cap)
    }
}

/// The only REAPI version the server supports, 2.0.
fn api_version() -> SemVer {
    SemVer {
        major: 2,
        minor: 0,
        patch: 0,
        ..Default::default()
    }
}
