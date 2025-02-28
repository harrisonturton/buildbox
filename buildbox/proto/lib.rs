//! Exposes all the proto generated types within meaningful namespaces.

pub mod bazel {
    pub mod asset {
        pub use remote_asset_proto::build::bazel::remote::asset::v1::fetch_server::*;
        pub use remote_asset_proto::build::bazel::remote::asset::v1::push_server::*;
        pub use remote_asset_proto::build::bazel::remote::asset::v1::*;
    }

    pub mod exec {
        pub use remote_execution_proto::build::bazel::remote::execution::v2::execution_server::*;
        pub use remote_execution_proto::build::bazel::remote::execution::v2::action_cache_server::*;
        pub use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::*;
        pub use remote_execution_proto::build::bazel::remote::execution::v2::capabilities_server::*;
        pub use remote_execution_proto::build::bazel::remote::execution::v2::*;
    }

    pub mod semver {
        pub use semver_proto::build::bazel::semver::*;
    }
}

pub mod google {
    pub use any_proto::google::protobuf;
    pub use operations_proto::google::longrunning;
    pub use status_proto::google::rpc;

    pub mod bytestream {
        pub use bytestream_proto::google::bytestream::*;
        pub use bytestream_proto::google::bytestream::byte_stream_server::*;
    }
}
