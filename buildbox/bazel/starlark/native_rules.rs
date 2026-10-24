//! The rules built into Bazel, rather than defined in `.bzl` files.

use crate::instantiate::instantiate;
use allocative::Allocative;
use bazel_packages::{AttrSchema, AttrType, AttrValue, RuleClass, RuleClassRef};
use intern::Symbol;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, starlark_value};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock};

/// The attributes every rule has.
pub fn common_attrs() -> Vec<AttrSchema> {
    vec![
        AttrSchema::new("name", AttrType::String).mandatory().nonconfigurable(),
        AttrSchema::new("visibility", AttrType::LabelList).nonconfigurable().not_dependency(),
        AttrSchema::new("tags", AttrType::StringList).nonconfigurable(),
        AttrSchema::new("testonly", AttrType::Bool).nonconfigurable(),
        AttrSchema::new("deprecation", AttrType::String),
        AttrSchema::new("features", AttrType::StringList),
        AttrSchema::new("licenses", AttrType::License).nonconfigurable(),
        AttrSchema::new("distribs", AttrType::StringList).nonconfigurable(),
        AttrSchema::new("compatible_with", AttrType::LabelList).nonconfigurable().not_dependency(),
        AttrSchema::new("restricted_to", AttrType::LabelList).nonconfigurable().not_dependency(),
        AttrSchema::new("target_compatible_with", AttrType::LabelList),
        AttrSchema::new("exec_compatible_with", AttrType::LabelList).nonconfigurable(),
        AttrSchema::new("exec_group_compatible_with", AttrType::StringListDict).nonconfigurable(),
        AttrSchema::new("toolchains", AttrType::LabelList),
        AttrSchema::new("exec_properties", AttrType::StringDict),
        AttrSchema::new("applicable_licenses", AttrType::LabelList).nonconfigurable(),
        AttrSchema::new("package_metadata", AttrType::LabelList).nonconfigurable(),
        AttrSchema::new("aspect_hints", AttrType::LabelList),
        AttrSchema::new("expect_failure", AttrType::String),
    ]
}

/// The attributes test rules have.
pub fn test_attrs() -> Vec<AttrSchema> {
    vec![
        AttrSchema::new("size", AttrType::String).default(AttrValue::String("medium".into())).nonconfigurable(),
        AttrSchema::new("timeout", AttrType::String).nonconfigurable(),
        AttrSchema::new("flaky", AttrType::Bool).nonconfigurable(),
        AttrSchema::new("shard_count", AttrType::Int).default(AttrValue::Int(-1)),
        AttrSchema::new("local", AttrType::Bool),
        AttrSchema::new("args", AttrType::StringList),
        AttrSchema::new("env", AttrType::StringDict),
        AttrSchema::new("env_inherit", AttrType::StringList),
    ]
}

/// The attributes executable rules have.
pub fn executable_attrs() -> Vec<AttrSchema> {
    vec![
        AttrSchema::new("args", AttrType::StringList),
        AttrSchema::new("env", AttrType::StringDict),
        AttrSchema::new("output_licenses", AttrType::StringList),
    ]
}

/// Make a rule class from its own attributes and the common ones. The
/// common attributes come first, then the class's own in the order given,
/// which is the order `query --output=build` prints them in.
pub fn rule_class(kind: &str, own: Vec<AttrSchema>, test: bool, executable: bool) -> RuleClass {
    let mut attrs = common_attrs();
    if test {
        attrs.extend(test_attrs());
    } else if executable {
        attrs.extend(executable_attrs());
    }
    for attr in own {
        match attrs.iter_mut().find(|existing| existing.name == attr.name) {
            Some(existing) => *existing = attr,
            None => attrs.push(attr),
        }
    }
    RuleClass {
        kind: Symbol::intern(kind),
        attrs,
        test,
        executable,
        outputs: Vec::new(),
    }
}

fn label(text: &str) -> AttrValue {
    AttrValue::Label(bazel_label::Label::parse_canonical(text).expect("built-in default labels are valid"))
}

fn native_classes() -> Vec<RuleClass> {
    use AttrType::*;
    let a = AttrSchema::new;
    vec![
        rule_class(
            "filegroup",
            vec![a("srcs", LabelList), a("data", LabelList), a("output_group", String)],
            false,
            false,
        ),
        rule_class(
            "genrule",
            vec![
                a("srcs", LabelList),
                a("outs", OutputList).mandatory(),
                a("cmd", String),
                a("cmd_bash", String),
                a("cmd_ps", String),
                a("cmd_bat", String),
                a("tools", LabelList).exec(),
                a("exec_tools", LabelList).exec(),
                a("executable", Bool).nonconfigurable(),
                a("local", Bool),
                a("message", String),
                a("output_licenses", StringList),
                a("output_to_bindir", Bool).nonconfigurable(),
                a("stamp", Tristate).default(AttrValue::Int(0)),
                a("$genrule_setup", Label).default(label("@@bazel_tools//tools/genrule:genrule-setup.sh")),
            ],
            false,
            false,
        ),
        rule_class("alias", vec![a("actual", Label).mandatory()], false, false),
        rule_class(
            "config_setting",
            vec![
                a("values", StringDict).nonconfigurable(),
                a("define_values", StringDict).nonconfigurable(),
                a("flag_values", LabelKeyedStringDict).nonconfigurable(),
                a("constraint_values", LabelList).nonconfigurable(),
            ],
            false,
            false,
        ),
        rule_class("test_suite", vec![a("tests", LabelList).nonconfigurable()], false, false),
        rule_class(
            "toolchain",
            vec![
                a("toolchain_type", Label).mandatory().nonconfigurable(),
                a("toolchain", Label).mandatory(),
                a("target_settings", LabelList).nonconfigurable(),
                a("use_target_platform_constraints", Bool).nonconfigurable(),
            ],
            false,
            false,
        ),
        rule_class("toolchain_type", vec![a("no_match_error", String)], false, false),
        rule_class(
            "constraint_setting",
            vec![a("default_constraint_value", Label).nonconfigurable().not_dependency()],
            false,
            false,
        ),
        rule_class(
            "constraint_value",
            vec![a("constraint_setting", Label).mandatory().nonconfigurable()],
            false,
            false,
        ),
        rule_class(
            "platform",
            vec![
                a("constraint_values", LabelList).nonconfigurable(),
                a("parents", LabelList).nonconfigurable(),
                a("remote_execution_properties", String),
                a("flags", StringList),
                a("required_settings", LabelList),
                a("missing_toolchain_error", String),
                a("check_toolchain_types", Bool),
                a("allowed_toolchain_types", LabelList),
            ],
            false,
            false,
        ),
        rule_class("label_flag", vec![a("build_setting_default", Label).mandatory()], false, false),
        rule_class("label_setting", vec![a("build_setting_default", Label).mandatory()], false, false),
        rule_class(
            "environment_group",
            vec![a("environments", LabelList).mandatory().not_dependency(), a("defaults", LabelList).mandatory().not_dependency()],
            false,
            false,
        ),
        rule_class("environment", vec![a("fulfills", LabelList).not_dependency()], false, false),
        rule_class(
            "genquery",
            vec![
                a("expression", String).mandatory(),
                a("scope", LabelList).mandatory().not_dependency(),
                a("opts", StringList),
                a("strict", Bool).default(AttrValue::Bool(true)),
                a("compressed_output", Bool),
            ],
            false,
            false,
        ),
        rule_class(
            "starlark_doc_extract",
            vec![
                a("src", Label).mandatory(),
                a("deps", LabelList),
                a("symbol_names", StringList),
                a("render_main_repo_name", Bool),
                a("_extractor", Label).default(label("@@bazel_tools//tools/build_defs:empty")),
            ],
            false,
            false,
        ),
    ]
}

/// The native rule classes, by kind.
pub static NATIVE_RULES: LazyLock<HashMap<Symbol, Arc<RuleClass>>> = LazyLock::new(|| {
    native_classes().into_iter().map(|class| (class.kind, Arc::new(class))).collect()
});

/// A native rule, as a callable Starlark value.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct NativeRule {
    #[allocative(skip)]
    pub class: Arc<RuleClass>,
}

starlark_simple_value!(NativeRule);

impl fmt::Display for NativeRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in rule {}>", self.class.kind)
    }
}

#[starlark_value(type = "rule")]
impl<'v> StarlarkValue<'v> for NativeRule {
    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let kwargs: Vec<(&str, Value<'v>)> = names.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        let class = RuleClassRef {
            kind: self.class.kind,
            definition: None,
        };
        instantiate(eval, class, &self.class, &kwargs).map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

/// Add every native rule to `builder`, under its kind.
pub fn register(builder: &mut GlobalsBuilder) {
    let mut classes: Vec<&Arc<RuleClass>> = NATIVE_RULES.values().collect();
    classes.sort_by(|a, b| a.kind.cmp(&b.kind));
    for class in classes {
        let value = builder.alloc(NativeRule { class: class.clone() });
        builder.set(class.kind.as_str(), value);
    }
}
