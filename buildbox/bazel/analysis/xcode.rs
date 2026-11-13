//! A stand-in for the Xcode configuration, which bb doesn't analyze.
//!
//! Rules read `ctx.attr._xcode_config[apple_common.XcodeVersionConfig]`
//! unconditionally, but only use it when building for Apple platforms. On
//! macOS, C++ toolchains ask it for the oldest macOS to build for, which
//! Bazel takes from `--macos_minimum_os` or else from the installed Xcode's
//! SDK. bb takes it from `--macos_minimum_os` alone, so a build doesn't
//! depend on which Xcode the host has. Other members are lenient opaque
//! values, as the whole configuration was before.

use allocative::Allocative;
use bazel_config::BuildOptions;
use bazel_starlark::opaque::Opaque;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// The `_xcode_config` attribute's target, and its `XcodeVersionConfig`
/// provider, which indexing the target with any provider gives.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct XcodeConfig {
    /// `--apple_platform_type`: the Apple platform a single-architecture
    /// build is for.
    platform_type: String,
    /// `--macos_minimum_os`, or `""` if it isn't given.
    macos_minimum_os: String,
}

starlark_simple_value!(XcodeConfig);

impl XcodeConfig {
    pub fn new(options: &BuildOptions) -> XcodeConfig {
        XcodeConfig {
            platform_type: options.string("apple_platform_type").to_string(),
            macos_minimum_os: options.string("macos_minimum_os").to_string(),
        }
    }

    /// The oldest version of the configuration's platform to build for.
    fn minimum_os(&self) -> anyhow::Result<&str> {
        if self.platform_type != "macos" {
            anyhow::bail!("building for Apple's {} platform isn't supported by bb yet", self.platform_type);
        }
        if self.macos_minimum_os.is_empty() {
            anyhow::bail!(
                "building for macOS with bb needs --macos_minimum_os, as bb doesn't read the installed Xcode's SDK version; \
                 set it in the project's .bazelrc, such as `build --macos_minimum_os=14.0`"
            );
        }
        Ok(&self.macos_minimum_os)
    }
}

impl fmt::Display for XcodeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<_xcode_config>")
    }
}

#[starlark_value(type = "XcodeVersionConfig")]
impl<'v> StarlarkValue<'v> for XcodeConfig {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("XcodeVersionConfig", xcode_config_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(Opaque::lenient(&format!("_xcode_config.{attribute}"))))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }

    /// The target's provider is the configuration itself.
    fn at(&self, _index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(XcodeConfig {
            platform_type: self.platform_type.clone(),
            macos_minimum_os: self.macos_minimum_os.clone(),
        }))
    }
}

#[starlark_module]
fn xcode_config_methods(builder: &mut MethodsBuilder) {
    /// The oldest version of a platform to build for. The platform type is
    /// always the configuration's own, from `--apple_platform_type`, as
    /// `ctx.fragments.apple.single_arch_platform.platform_type` gives it.
    fn minimum_os_for_platform_type<'v>(this: Value<'v>, platform_type: Value<'v>) -> anyhow::Result<String> {
        let _ = platform_type;
        let config = this.downcast_ref::<XcodeConfig>().ok_or_else(|| anyhow::anyhow!("expected an XcodeVersionConfig"))?;
        Ok(config.minimum_os()?.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(platform_type: &str, macos_minimum_os: &str) -> XcodeConfig {
        XcodeConfig {
            platform_type: platform_type.to_string(),
            macos_minimum_os: macos_minimum_os.to_string(),
        }
    }

    #[test]
    fn gives_the_macos_minimum_os_option() {
        assert_eq!(config("macos", "14.0").minimum_os().unwrap(), "14.0");
    }

    #[test]
    fn asks_for_the_macos_minimum_os_option_when_it_isnt_given() {
        let error = config("macos", "").minimum_os().unwrap_err().to_string();
        assert!(error.contains("--macos_minimum_os"), "{error}");
    }

    #[test]
    fn refuses_other_apple_platforms() {
        let error = config("ios", "14.0").minimum_os().unwrap_err().to_string();
        assert!(error.contains("ios"), "{error}");
    }
}
