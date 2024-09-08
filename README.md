## `build-server`

Implementation of the Bazel remote execution API that runs on a single machine.

### Development

The Bazel toolchains assume the existence of certain binaries on the build host.
These are provided by the `shell.nix` file, so you must step into that nix shell
before running the build:

```
nix-shell
```

This will make the required binaries available on the `$PATH` and discoverable
by the relevant module extensions.