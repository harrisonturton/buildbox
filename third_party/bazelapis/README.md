# //third_party/bazelapis

Bazel's own protos that bb speaks: the Build Event Protocol
(`build_event_stream.proto`) and the protos it imports. They are copied
unchanged from Bazel 9.3.1, under the Apache License 2.0, at the paths Bazel
imports them by, so `strip_import_prefix` keeps those import paths.
