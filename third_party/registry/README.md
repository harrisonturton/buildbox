# registry

Local Bazel registry for bringing in third-party dependencies. When a dependency
is added in `MODULE.bazel` it will first check for the dependency and version
here, before going to the Bazel Central Registry (BCR).

This behaviour is configured in `.bazelrc`:

```
# Use local registry for third-party dependencies
common --registry=file://%workspace%/third_party/registry
common --registry=https://bcr.bazel.build
```

## Integrity SHA256 sums

Bazel doesn't like the plain `sha256sum` output. Instead, we can use:

```
cat <file> | openssl dgst -sha256 -binary | openssl base64 -A | pbcopy
```

To copy the SHA256 hash to your clipboard.