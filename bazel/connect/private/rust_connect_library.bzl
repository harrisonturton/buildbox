"""`rust_connect_library`: a Rust crate generated from proto_library targets.

The codegen driver reads the descriptor sets proto_library already produces,
so protoc runs once per proto (inside proto_library) and no protoc plugins
are involved. Output is a single `lib.rs` holding one `pub mod` per proto
package segment, so `build.bazel.remote.execution.v2` lands at
`<crate>::build::bazel::remote::execution::v2`.
"""

load("@protobuf//bazel/common:proto_info.bzl", "ProtoInfo")
load("@rules_rust//rust:defs.bzl", "rust_library")

visibility(["//bazel/connect/..."])

# Crates the generated code names by absolute path. buffa-types holds the
# well-known types (google.protobuf.*).
_RUNTIME_DEPS = [
    "@crates//:buffa",
    "@crates//:buffa-types",
    "@crates//:connectrpc",
    "@crates//:serde",
    "@crates//:serde_json",
]

def _rust_connect_codegen_impl(ctx):
    protos = [p[ProtoInfo] for p in ctx.attr.protos]
    descriptor_sets = depset(transitive = [p.transitive_descriptor_sets for p in protos])
    out = ctx.actions.declare_file(ctx.label.name + "/lib.rs")

    args = ctx.actions.args()
    args.add("--out", out)
    args.add_all([p.direct_descriptor_set for p in protos], before_each = "--generate")
    args.add_all(descriptor_sets, before_each = "--descriptor_set")

    ctx.actions.run(
        executable = ctx.executable._codegen,
        arguments = [args],
        inputs = descriptor_sets,
        outputs = [out],
        mnemonic = "RustConnectCodegen",
        progress_message = "Generating Connect Rust for %{label}",
    )
    return [DefaultInfo(files = depset([out]))]

_rust_connect_codegen = rule(
    implementation = _rust_connect_codegen_impl,
    doc = "Generates the `lib.rs` of a rust_connect_library.",
    attrs = {
        "protos": attr.label_list(
            providers = [ProtoInfo],
            mandatory = True,
        ),
        "_codegen": attr.label(
            default = "//bazel/connect/codegen",
            executable = True,
            cfg = "exec",
        ),
    },
)

def rust_connect_library(name, protos, crate_name = None, visibility = None, **kwargs):
    """Buffa message types plus Connect service traits, servers and clients.

    Code is generated for the `.proto` files directly in `protos`. Their
    imports must resolve within the same crate, so list every proto_library
    whose types are referenced.

    Args:
      name: The rust_library's name, and its crate name unless `crate_name`
        is given.
      protos: proto_library targets to generate code for.
      crate_name: Overrides the crate name.
      visibility: Visibility of the rust_library.
      **kwargs: Passed to both targets (`tags`, `testonly`, ...).
    """
    codegen = name + "_codegen"
    _rust_connect_codegen(
        name = codegen,
        protos = protos,
        visibility = ["//visibility:private"],
        **kwargs
    )
    rust_library(
        name = name,
        srcs = [":" + codegen],
        crate_name = crate_name,
        edition = "2024",
        visibility = visibility,
        deps = _RUNTIME_DEPS,
        **kwargs
    )
