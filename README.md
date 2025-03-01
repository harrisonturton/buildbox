## `buildbox`

Intended to be a simple, single-server implementation of the Bazel remote
execution API. Not functional yet.

## TLS configuration

To use self-signed certificates for testing:

```
openssl req -new -newkey rsa:4096 -x509 -sha256 -days 3650 -nodes -batch -subj "/CN=localhost" -out server.crt -keyout server.key
openssl req -new -newkey rsa:4096 -x509 -sha256 -days 3650 -nodes -batch -subj "/CN=client" -out client.crt -keyout client.key
```

The `server.crt` and `server.key` are used by the `build-server`, and the
`client.crt` and `client.key` are used by Bazel. For example, they can be added
to your `.bazelrc` like:

```
build --tls_certificate="server.crt"
build --tls_client_certificate="client.crt"
build --tls_client_key="client.key"
```

## `rust-project.json`

To generate the `rust-project.json` file, which is needed for IDE support, run:

```
./tools/regen_rust_project.sh
```

Which, under the hood, runs:

```
RUST_LOG=trace bazel run @rules_rust//tools/rust_analyzer:gen_rust_project -- //server/...
```

Note the `//server/...` part of the query is needed so that the
`gen_rust_project` script only picks up dependencies from `//server`. Without
it, the command will crawl all directories, and break when it gets to
`//third_party` because it will encounter `BUILD.bazel` files who have labels
that are relative to the third-party module, not the repository root.

Also potentially helpful:

```
bazel build //server --aspects @rules_rust//rust:defs.bzl%rust_analyzer_aspect --output_groups=rust_analyzer_crate_spec,rust_generated_srcs
bazel aquery --include_aspects --include_artifacts --aspects @rules_rust//rust:defs.bzl%rust_analyzer_aspect --output_groups=rust_analyzer_crate_spec 'outputs(".*\.rust_analyzer_crate_spec\.json", deps(//server))' --output=jsonproto
``
