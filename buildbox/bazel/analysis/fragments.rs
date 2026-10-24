//! `ctx.fragments` and `ctx.configuration`: parts of the configuration
//! rules read.

use allocative::Allocative;
use bazel_starlark::opaque::Opaque;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// What rules read of the configuration, by option.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub compilation_mode: String,
    pub action_env: Vec<(String, String)>,
    /// Each option `config_setting(values = ...)` matches, such as `copt`.
    pub values: Vec<(String, String)>,
    pub is_exec: bool,
}

impl Options {
    fn words(&self, option: &str) -> Vec<String> {
        let value = self.values.iter().find(|(name, _)| name == option).map(|(_, value)| value.as_str()).unwrap_or_default();
        value.split(' ').filter(|word| !word.is_empty()).map(str::to_string).collect()
    }
}

/// `ctx.fragments`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Fragments(#[allocative(skip)] pub Options);

starlark_simple_value!(Fragments);

impl fmt::Display for Fragments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<fragments>")
    }
}

#[starlark_value(type = "fragments")]
impl<'v> StarlarkValue<'v> for Fragments {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(Fragment {
            name: attribute.to_string(),
            options: self.0.clone(),
        }))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

/// A configuration fragment, such as `cpp`. Members bb doesn't know are
/// opaque.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Fragment {
    pub name: String,
    #[allocative(skip)]
    pub options: Options,
}

starlark_simple_value!(Fragment);

impl fmt::Display for Fragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} fragment>", self.name)
    }
}

#[starlark_value(type = "fragment")]
impl<'v> StarlarkValue<'v> for Fragment {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("fragment", fragment_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let words = |option: &str| heap.alloc(AllocList(self.options.words(option)));
        let value = match (self.name.as_str(), attribute) {
            ("cpp", "copts") => words("copt"),
            ("cpp", "cxxopts") => words("cxxopt"),
            ("cpp", "conlyopts") => words("conlyopt"),
            ("cpp", "linkopts") => words("linkopt"),
            ("cpp", "compilation_mode") | ("cpp", "_compilation_mode") => heap.alloc(self.options.compilation_mode.as_str()),
            ("proto", "experimental_protoc_opts") => words("protocopt"),
            ("proto", "cc_proto_library_header_suffixes") => heap.alloc(AllocList([".pb.h"])),
            ("proto", "cc_proto_library_source_suffixes") => heap.alloc(AllocList([".pb.cc"])),
            ("apple", "prefer_mutual_xcode") => Value::new_bool(true),
            ("apple", "include_xcode_exec_requirements") => Value::new_bool(false),
            ("apple", flag) if flag.ends_with("_flag") => Value::new_none(),
            _ => heap.alloc(Opaque::new(&format!("ctx.fragments.{}.{attribute}", self.name))),
        };
        Some(value)
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

/// Fragment methods that answer as Bazel's do with default options.
#[starlark_module]
fn fragment_methods(builder: &mut MethodsBuilder) {
    fn fission_active_for_current_compilation_mode<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn force_pic<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `cpp.minimum_os_version()`: `--minimum_os_version`, unset.
    fn minimum_os_version<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn generate_llvm_lcov<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn should_strip_binaries<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let fragment = this.downcast_ref::<Fragment>().ok_or_else(|| anyhow::anyhow!("expected a fragment"))?;
        Ok(fragment.options.compilation_mode == "fastbuild")
    }
}

/// `ctx.configuration`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Configuration(#[allocative(skip)] pub Options);

starlark_simple_value!(Configuration);

impl fmt::Display for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<configuration>")
    }
}

#[starlark_value(type = "configuration")]
impl<'v> StarlarkValue<'v> for Configuration {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("configuration", configuration_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let value = match attribute {
            "coverage_enabled" => Value::new_bool(false),
            "host_path_separator" => heap.alloc(":"),
            "default_shell_env" => heap.alloc(AllocDict(self.0.action_env.iter().map(|(name, value)| (name.as_str(), value.as_str())))),
            "test_env" => heap.alloc(AllocDict(Vec::<(&str, &str)>::new())),
            _ => return None,
        };
        Some(value)
    }

    fn dir_attr(&self) -> Vec<String> {
        ["coverage_enabled", "host_path_separator", "default_shell_env", "test_env"].iter().map(|name| name.to_string()).collect()
    }
}

#[starlark_module]
fn configuration_methods(builder: &mut MethodsBuilder) {
    fn stamp_binaries<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let configuration = this.downcast_ref::<Configuration>().ok_or_else(|| anyhow::anyhow!("expected a configuration"))?;
        Ok(configuration.0.values.iter().any(|(name, value)| name == "stamp" && value == "true"))
    }

    fn is_tool_configuration<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let configuration = this.downcast_ref::<Configuration>().ok_or_else(|| anyhow::anyhow!("expected a configuration"))?;
        Ok(configuration.0.is_exec)
    }

    fn runfiles_enabled<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }
}
