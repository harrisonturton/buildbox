"""Public API for the Connect RPC rules. Load everything from here.

    rust_connect_library  a Rust crate of message types, service traits,
                          servers and clients generated from proto_library
                          targets
"""

load("//bazel/connect/private:rust_connect_library.bzl", _rust_connect_library = "rust_connect_library")

rust_connect_library = _rust_connect_library
