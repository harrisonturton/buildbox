# `buildbox`

Buildbox is an implementation of the Bazel [Remote Execution](https://bazel.build/remote/rbe)
protocol.

It's designed for near-trivial deployment and maintenance. Getting started is as simple as:

```
buildbox up
```

Buildbox is intended for personal use. I developed it to achieve easy
reproducible/hermetic builds at home, and also to make it easy to develop
applications that use Linux-based toolchains (e.g. Vivado) from my Macbook.

This may be useful to you if:

* You want remote execution without the yak shaving
* You trust your compilation inputs (i.e. don't need to isolate build actions)
* You're happy with *mostly* hermetic and reproducible
* You don't care about scale

This is a personal utility that I've made available to the world in case others
find it useful. I'll gladly accept contributions, but I make no stability,
support, or response time guarantees.

## How it works

There are two main parts to the remote execution protocol:

1. Action caching (i.e. storing the outputs of already-executed actions)
2. Action execution

Buildbox implements these in the simplest way possible.

Whenever the protocol requests something to be stored, the corresponding blob is
stored in a single file on-disk, named according to the SHA256 hash of the data.

Whenever an action needs to be executed, buildbox creates a new sandbox
directory `/sandbox-{id}/` and populates it with the necessary inputs copied
from the file blobs. The build command is then executed within this directory,
and the outputs are copied out into content-addressable blobs.

This architecture is simple and *sufficiently* fast. Because there is no action
isolation, they execute quickly. Because the execroots are populated from files
already stored on-disk, constructing the trees is also quick enough. There are
fancier optimisations we could do here (i.e. use FUSE or NFS filesystems to
project execroot views without copying files) but they'd cause an increase in
deployment complexity.

The big downside, of course, is that there's no security guardrails. Please only
use this in trusted environments.

## Building

If you already have Bazel, building `buildbox` is easy:

```
git clone git@github.com:harrisonturton/buildbox.git
cd buildbox
bazel build //buildbox
```

This will produce the `buildbox` binary at:

```
$(bazel info bazel-bin)/buildbox/buildbox
```

If you don't already have Bazel installed, you probably shouldn't be reading this `README`.


## Usage

### Server setup

> **Note:** Buildbox does not use TLS. This makes it very easy to
> setup and run, but it's important that the server is not exposed to the
> internet. I recommend exposing it to your local network over something like
> [Tailscale](https://tailscale.com).

Starting the buildbox server is easy:

```
buildbox up
```

By default, this will listen on `localhost:50051` and create a `~/.buildbox`
directory to store compilation artifacts and directories for executing actions.

If you'd rather put these files somewhere else, you can configure this behaviour
using a small configuration file. The configuration file is canonically called
`buildbox.toml` (but can be anything) and the most important directives are the
*storage directory* and the *sandbox directory*.

```
buildbox up --config <config file path>
```

The storage directory will contain cached build-time artifacts. The sandbox
directory will contain subdirectories that create the environments required to
execute build actions and extract their outputs.

A possible `buildbox.toml` could be:

```
addr        = "[::1]:50051"
storage_dir = "~/.my-custom-buildbox-dir/storage"
sandbox_dir = "~/.my-custom-buildbox-dir/sandbox"
```

## Client setup

To use that server with Bazel, you can configure the connection in your
`.bazelrc` like so:

```
build --remote_executor="grpc://localhost:50051"
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
