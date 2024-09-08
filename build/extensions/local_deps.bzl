"""
Find local dependencies and create the @local repository.

Binaries are resolved from the $PATH.
"""

load(":local_repo.bzl", "local_repo")

def _find_binary_path(ctx, name):
    """Get the path (as a string) of a binary on the $PATH"""
    file = ctx.which(name)
    if file == None:
        fail("could not find {} binary".format(name))
    return "{}".format(file)

def _local_deps_impl(ctx):
    local_repo(
        name = "local",
        protoc = _find_binary_path(ctx, "protoc"),
        protoc_gen_prost = _find_binary_path(ctx, "protoc-gen-prost"),
        protoc_gen_tonic = _find_binary_path(ctx, "protoc-gen-tonic"),
    )

local_deps = module_extension(
    implementation = _local_deps_impl,
    tag_classes = {
        "symlink_all": tag_class(),
    },
)
