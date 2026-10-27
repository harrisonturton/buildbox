//! The labels `configuration_field()` defaults name in a configuration, as
//! Bazel's configuration fragments give them.

use bazel_config::BuildOptions;
use bazel_label::Label;
use bazel_packages::{AttrValue, LateBound};
use skyframe::SkyError;

/// A late-bound default's value under `options`: the label it names, or
/// none.
pub fn late_bound_value(late_bound: &LateBound, options: &BuildOptions) -> Result<AttrValue, SkyError> {
    let value = match late_bound_label(late_bound, options)? {
        Some(label) => AttrValue::Label(label),
        None => AttrValue::None,
    };
    Ok(value)
}

/// The label a late-bound default names under `options`, if any. A field
/// bb doesn't know fails, rather than leaving the attribute empty.
fn late_bound_label(late_bound: &LateBound, options: &BuildOptions) -> Result<Option<Label>, SkyError> {
    let label = |option: &str| options.label(option);
    let label = match (late_bound.fragment.as_str(), late_bound.name.as_str()) {
        ("proto", "proto_compiler") => label("proto_compiler"),
        ("proto", "proto_toolchain_for_cc") => label("proto_toolchain_for_cc"),
        ("proto", "proto_toolchain_for_java") => label("proto_toolchain_for_java"),
        ("proto", "proto_toolchain_for_java_lite") => label("proto_toolchain_for_javalite"),
        ("proto", "proto_toolchain_for_j2objc") => label("proto_toolchain_for_j2objc"),
        ("coverage", "output_generator") if options.bool("collect_code_coverage") => label("coverage_output_generator"),
        ("coverage", "output_generator") => None,
        ("java", "launcher") => label("java_launcher"),
        ("java", "proguard_top") => label("proguard_top"),
        ("java", "bytecode_optimizer") => bytecode_optimizer(options)?,
        ("java", "java_toolchain_bytecode_optimizer") => java_optimization(options, "bytecode_optimizer")?,
        ("java", "local_java_optimization_configuration") => java_optimization(options, "experimental_local_java_optimization_configuration")?,
        ("cpp", "libc_top") => label("grte_top"),
        ("apple", "xcode_config_label") => xcode_config(options)?,
        ("cpp", name) if FDO_FIELDS.contains(&name) => None,
        (fragment, name) => {
            return Err(SkyError::new(format!(
                "configuration_field(fragment = \"{fragment}\", name = \"{name}\") isn't supported by bb yet"
            )));
        }
    };
    Ok(label)
}

/// The `cpp` fragment's fields that only name labels when options for
/// profile-guided optimization are given, which bb doesn't support.
const FDO_FIELDS: &[&str] = &[
    "zipper",
    "custom_malloc",
    "fdo_optimize",
    "fdo_prefetch_hints",
    "fdo_profile",
    "cs_fdo_profile",
    "propeller_optimize",
    "xbinary_fdo",
    "memprof_profile",
    "proto_profile_path",
];

/// The Xcode configuration, which bb doesn't analyze: rules get a stand-in
/// for its target, which only matters when building for Apple platforms,
/// so naming another with `--xcode_version_config` isn't supported.
fn xcode_config(options: &BuildOptions) -> Result<Option<Label>, SkyError> {
    let changed = options.changed().iter().any(|(name, _)| name == "xcode_version_config");
    match changed {
        true => Err(SkyError::new("--xcode_version_config isn't supported by bb yet")),
        false => Ok(None),
    }
}

/// The label `--experimental_bytecode_optimizers` gives its one
/// optimizer, which by default names none.
fn bytecode_optimizer(options: &BuildOptions) -> Result<Option<Label>, SkyError> {
    match options.string("bytecode_optimizers").contains('=') {
        true => Err(SkyError::new("--experimental_bytecode_optimizers with a label isn't supported by bb yet")),
        false => Ok(None),
    }
}

/// A field of the `java` fragment that only names a label with
/// `--experimental_local_java_optimizations`, which bb doesn't support.
fn java_optimization(options: &BuildOptions, what: &str) -> Result<Option<Label>, SkyError> {
    match options.bool("experimental_local_java_optimizations") {
        true => Err(SkyError::new(format!("--experimental_local_java_optimizations isn't supported by bb yet, so neither is its {what}"))),
        false => Ok(None),
    }
}
