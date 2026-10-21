# remoteapis

Protos from the Bazel Remote Execution API,
<https://github.com/bazelbuild/remote-apis>, vendored as a plain package.

The upstream module isn't published to the Bazel Central Registry, and
depending on it directly pulls grpc, rules_go and rules_foreign_cc into module
resolution. The protos keep their upstream paths under this directory, and
`strip_import_prefix` makes them importable as `build/bazel/...`.

This directory has no `.MODULE.bazel` file of its own: the protos' dependencies
(`googleapis` and `protobuf`) are declared in their own `third_party/`
directories.
