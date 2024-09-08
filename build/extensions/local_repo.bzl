"""
Create repository for local files.

These files are external to Bazel and assumed to be present in the runtime
environment. They can be referred to like `@local//:protoc`.
"""

_LOCAL_REPO_BUILD = """
package(
    default_visibility = ["//visibility:public"]
)

exports_files([
  "protoc",
  "protoc_gen_prost",
  "protoc_gen_tonic",
])
"""

def _local_repo_impl(ctx):
    ctx.symlink(ctx.attr.protoc, "protoc")
    ctx.symlink(ctx.attr.protoc_gen_prost, "protoc_gen_prost")
    ctx.symlink(ctx.attr.protoc_gen_tonic, "protoc_gen_tonic")
    ctx.file("BUILD.bazel", content = _LOCAL_REPO_BUILD)

local_repo = repository_rule(
    implementation = _local_repo_impl,
    attrs = {
        "protoc": attr.string(
            doc = "Path to the protoc binary on the host",
            mandatory = True,
        ),
        "protoc_gen_prost": attr.string(
            doc = "Path to the protoc-gen-prost binary on the host",
            mandatory = True,
        ),
        "protoc_gen_tonic": attr.string(
            doc = "Path to the protoc-gen-tonic binary on the host",
            mandatory = True,
        ),
    },
)