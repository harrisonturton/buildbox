//! `config`: the types of build settings, which `rule(build_setting =
//! ...)` declares, and the configurations of attributes' dependencies,
//! which `attr.label(cfg = ...)` chooses.

use allocative::Allocative;
use bazel_packages::BuildSettingSpec;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::none::NoneOr;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, starlark_value};
use std::fmt;

/// A build setting's type: `config.string(flag = True)`, and so on.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct BuildSettingType {
    /// What Bazel calls the type, such as `string` or `string_list`.
    pub name: String,
    /// Whether the command line can set it.
    pub flag: bool,
    /// Whether each use of the flag adds a value to a list.
    pub multiple: bool,
}

impl BuildSettingType {
    pub fn spec(&self) -> BuildSettingSpec {
        BuildSettingSpec {
            flag: self.flag,
            multiple: self.multiple,
        }
    }
}

starlark_simple_value!(BuildSettingType);

impl fmt::Display for BuildSettingType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<config.{}>", self.name)
    }
}

#[starlark_value(type = "BuildSetting")]
impl<'v> StarlarkValue<'v> for BuildSettingType {}

/// Where an attribute's dependencies are built: `config.exec(...)` for the
/// execution platform, `config.target()` for the rule's own configuration,
/// or `config.none()` for none.
#[derive(Debug, Clone, PartialEq, Eq, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct ConfigChoice {
    pub kind: ConfigKind,
    /// For `config.exec`: the exec group whose platform it is.
    pub exec_group: Option<String>,
}

/// Which configuration a [`ConfigChoice`] chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative, StarlarkPagable)]
pub enum ConfigKind {
    Exec,
    Target,
    None,
}

starlark_simple_value!(ConfigChoice);

impl fmt::Display for ConfigChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.kind {
            ConfigKind::Exec => "exec",
            ConfigKind::Target => "target",
            ConfigKind::None => "none",
        };
        write!(f, "<config.{name}>")
    }
}

#[starlark_value(type = "config")]
impl<'v> StarlarkValue<'v> for ConfigChoice {}

fn setting(name: &str, flag: bool, multiple: bool) -> BuildSettingType {
    BuildSettingType {
        name: name.to_string(),
        flag,
        multiple,
    }
}

#[starlark_module]
fn config_functions(builder: &mut GlobalsBuilder) {
    fn bool(#[starlark(require = named, default = false)] flag: bool) -> anyhow::Result<BuildSettingType> {
        Ok(setting("bool", flag, false))
    }

    fn int(#[starlark(require = named, default = false)] flag: bool) -> anyhow::Result<BuildSettingType> {
        Ok(setting("int", flag, false))
    }

    /// `config.string(flag = False, allow_multiple = False)`: with
    /// `allow_multiple`, each use of the flag adds a string to a list.
    fn string(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] allow_multiple: bool,
    ) -> anyhow::Result<BuildSettingType> {
        Ok(setting("string", flag, allow_multiple))
    }

    /// `config.string_list(flag = False, repeatable = False)`: with
    /// `repeatable`, each use of the flag adds a string, rather than giving
    /// the list separated by commas.
    fn string_list(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] repeatable: bool,
    ) -> anyhow::Result<BuildSettingType> {
        Ok(setting("string_list", flag, repeatable))
    }

    fn string_set(
        #[starlark(require = named, default = false)] flag: bool,
        #[starlark(require = named, default = false)] repeatable: bool,
    ) -> anyhow::Result<BuildSettingType> {
        Ok(setting("string_set", flag, repeatable))
    }

    fn exec(#[starlark(default = NoneOr::None)] exec_group: NoneOr<String>) -> anyhow::Result<ConfigChoice> {
        Ok(ConfigChoice {
            kind: ConfigKind::Exec,
            exec_group: exec_group.into_option(),
        })
    }

    fn target() -> anyhow::Result<ConfigChoice> {
        Ok(ConfigChoice {
            kind: ConfigKind::Target,
            exec_group: None,
        })
    }

    fn none() -> anyhow::Result<ConfigChoice> {
        Ok(ConfigChoice {
            kind: ConfigKind::None,
            exec_group: None,
        })
    }
}

/// Add the `config` namespace.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("config", config_functions);
}
