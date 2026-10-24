# //bazel/connect

[Connect RPC](https://github.com/connectrpc/connect-rust) for Rust, generated
from `proto_library` targets: [buffa](https://github.com/anthropics/buffa)
message types, service traits, servers and clients. Servers speak gRPC,
gRPC-Web and the Connect protocol from a single registration, so Bazel's gRPC
client and a plain HTTP/1.1 client can both call them.

```starlark
load("@protobuf//bazel:proto_library.bzl", "proto_library")
load("//bazel/connect:defs.bzl", "rust_connect_library")

proto_library(
    name = "admin_proto",
    srcs = ["admin.proto"],
)

rust_connect_library(
    name = "admin_rust_connect",
    protos = [":admin_proto"],
)
```

```rust
use admin_rust_connect::buildbox::admin::v1::{Admin, AdminClient, GetStatusRequest};
```

| Rule | |
| --- | --- |
| `rust_connect_library` | A `rust_library` generated from the `.proto` files directly in `protos`, one `pub mod` per package segment. |

Code is generated only for the protos listed directly in `protos`, and their
types must resolve within the one crate, so list every `proto_library` whose
types are referenced. The well-known types (`google.protobuf.*`) are the
exception: they come from the `buffa-types` crate. Protos that are only
imported for their options, such as `google/api/annotations.proto`, needn't be
listed.

## How it works

`proto_library` already runs protoc and emits descriptor sets. `codegen/` is
a small Rust binary over `connectrpc-codegen` that reads those sets and writes
a single `lib.rs`. So there are no protoc plugins, no `build.rs`, and no
checked-in generated code. Everything, the codegen binary included, builds
from crates pinned in `//third_party/crates`.

## Layout

- `defs.bzl`: the public API. Load nothing else.
- `private/`: rule implementations, load-visible only within `//bazel/connect/...`.
- `codegen/`: the descriptor-set-to-`lib.rs` driver.

Adapted from the rules of the same name in neptune, without the TypeScript
half.
