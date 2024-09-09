pub mod remote_asset {
    pub use remote_asset_proto::build::bazel::remote::asset::v1::fetch_server::*;
    pub use remote_asset_proto::build::bazel::remote::asset::v1::push_server::*;
    pub use remote_asset_proto::build::bazel::remote::asset::v1::*;
}

pub mod remote_execution {
    pub use remote_execution_proto::build::bazel::remote::execution::v2::execution_server::*;
    pub use remote_execution_proto::build::bazel::remote::execution::v2::action_cache_server::*;
    pub use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::*;
    pub use remote_execution_proto::build::bazel::remote::execution::v2::capabilities_server::*;
    pub use remote_execution_proto::build::bazel::remote::execution::v2::*;
}
