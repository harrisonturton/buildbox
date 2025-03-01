# `buildbox`

> While this is a fully-functional remote execution server, it's still firmly
> in development. It is ready for personal, non-production use only.

Buildbox is a simple implementation of the Bazel [Remote
Execution](https://bazel.build/remote/rbe) protocol. It consists of a single
executable that bundles a server and commandline client.

It is intended for personal use. I developed buildbox to achieve easy
reproducible/hermetic builds at home, and also to make it easy to develop
applications that use Linux-based toolchains (e.g. Vivado) from my Macbook.

This may be useful to you if:

* You don't care about scale
* You want remote execution without the yak shaving
* You trust your compilation inputs (i.e. don't need to isolate build actions)
* You're happy with *mostly* hermetic and reproducible

## Usage

### Server setup

> **Note:** Buildbox does not use TLS. This makes it very easy to
> setup and run, but it's important that the server is not exposed to the
> internet. I recommend exposing it to your local network over something like
> [Tailscale](https://tailscale.com).

To run buildbox, you'll need a small configuration file, `buildbox.toml`. This
is where you can configure a lot of behaviour, but the most important part is
identifying the *storage directory* and *sandbox directory*.

The storage directory will contain the files used to implement the content
addressible storage service required by the remote execution protocol. The
sandbox directory is where buildbox will construct the filetrees to execute
build actions and extract their outputs.

A possible `buildbox.toml` could be:

```
addr     = "[::1]:50051"
cachedir = "~/.buildbox/storage"
execdir  = "~/.buildbox/sandbox"
```

This tells the `buildbox` gRPC server to listen on `:50051`, and to store files
and create sandboxes within the `.buildbox` folder in your home directory. You
might like to put this file at `~/.buildbox/buildbox.toml`.

To start the server, if you're currently in the directory containing the
`buildbox.toml`, you can run:

```
buildbox up
```

But if your configuration file is placed somewhere else, you can specify the path with:

```
buildbox up --config <config file path>
```

## Client setup

To use that server with Bazel, you can configure the connection in your
`.bazelrc` like so:

```
>build --remote_executor="grpc://localhost:50051"
```

And that's it! Your Bazel builds will now be passed to the buildbox server. You'll need to make sure that your toolchains work with the server's environment, but once you've done that, it should "just work".

You can use the `buildbox` binary as a client to adminster the server. If the service is running on `:50051`, it will work by default, but you can pass the address in explicitly using `--addr`.

```
> buildbox blobs
NAME
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
cd4d9df7f4e22659e33bb204fc1a7cd8577df7bfcc046bdb61fc704187f4abdc
e5be7e5560520731e1e8f57d1e6447cfcb5c8656e7c26fc630c99caa62de1177
...
```

## Development

### Rust Analyzer

Because this repository is built with Bazel, rust analyzer cannot index it
without some help. We need to provide it with the `rust-project.json` file,
which we can do using the following utility script:

```
./tools/regen_rust_project.sh
```

Which, under the hood, runs:

```
RUST_LOG=trace bazel run @rules_rust//tools/rust_analyzer:gen_rust_project -- //buildbox/...
```

Note the `//buildbox/...` part of the query is needed so that the
`gen_rust_project` script only picks up dependencies from `//buildbox`. Without
it, the command will crawl all directories, and break when it gets to
`//third_party` because it will encounter `BUILD.bazel` files who have labels
that are relative to the third-party module, not the repository root.

Also potentially helpful:

```
bazel build //buildbox --aspects @rules_rust//rust:defs.bzl%rust_analyzer_aspect --output_groups=rust_analyzer_crate_spec,rust_generated_srcs
bazel aquery --include_aspects --include_artifacts --aspects @rules_rust//rust:defs.bzl%rust_analyzer_aspect --output_groups=rust_analyzer_crate_spec 'outputs(".*\.rust_analyzer_crate_spec\.json", deps(//server))' --output=jsonproto
```