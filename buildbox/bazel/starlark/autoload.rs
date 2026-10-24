//! Bazel 8's autoloads: rules that moved out of Bazel to rule sets, which
//! BUILD files can still use without loading them.

/// Each autoloaded symbol, the repository it's loaded from (by its name in
/// `@bazel_tools`), and the file within it.
pub const AUTOLOADS: &[(&str, &str, &str)] = &[
    ("sh_binary", "rules_shell", "//shell:sh_binary.bzl"),
    ("sh_library", "rules_shell", "//shell:sh_library.bzl"),
    ("sh_test", "rules_shell", "//shell:sh_test.bzl"),
    ("py_binary", "rules_python", "//python:py_binary.bzl"),
    ("py_library", "rules_python", "//python:py_library.bzl"),
    ("py_test", "rules_python", "//python:py_test.bzl"),
    ("py_runtime", "rules_python", "//python:py_runtime.bzl"),
    ("py_runtime_pair", "rules_python", "//python:py_runtime_pair.bzl"),
    ("java_binary", "rules_java", "//java:java_binary.bzl"),
    ("java_import", "rules_java", "//java:java_import.bzl"),
    ("java_library", "rules_java", "//java:java_library.bzl"),
    ("java_plugin", "rules_java", "//java:java_plugin.bzl"),
    ("java_test", "rules_java", "//java:java_test.bzl"),
    ("java_package_configuration", "rules_java", "//java/toolchains:java_package_configuration.bzl"),
    ("java_runtime", "rules_java", "//java/toolchains:java_runtime.bzl"),
    ("java_toolchain", "rules_java", "//java/toolchains:java_toolchain.bzl"),
    ("proto_library", "com_google_protobuf", "//bazel:proto_library.bzl"),
    ("cc_proto_library", "com_google_protobuf", "//bazel:cc_proto_library.bzl"),
    ("java_proto_library", "com_google_protobuf", "//bazel:java_proto_library.bzl"),
    ("java_lite_proto_library", "com_google_protobuf", "//bazel:java_lite_proto_library.bzl"),
    ("proto_lang_toolchain", "com_google_protobuf", "//bazel/toolchains:proto_lang_toolchain.bzl"),
    ("proto_toolchain", "com_google_protobuf", "//bazel/private:proto_toolchain_rule.bzl"),
];

/// The autoloaded symbols a BUILD file's source calls without loading.
pub fn used_autoloads(source: &str, loaded: &[String]) -> Vec<&'static (&'static str, &'static str, &'static str)> {
    AUTOLOADS
        .iter()
        .filter(|(symbol, _, _)| !loaded.iter().any(|name| name == symbol) && calls(source, symbol))
        .collect()
}

/// Whether `source` calls `symbol` as a function at the start of a line, as
/// rule instantiations are written.
fn calls(source: &str, symbol: &str) -> bool {
    source.lines().any(|line| line.trim_start().strip_prefix(symbol).is_some_and(|rest| rest.trim_start().starts_with('(')))
}
