//! Whether the conditions of `select()`s match a configuration.

use crate::attrs::resolve_value;
use crate::platform::{ConstraintValueKey, PlatformKey, labels};
use bazel_config::{BuildConfiguration, ConfigId};
use bazel_label::Label;
use bazel_loading::PackageKey;
use bazel_packages::{AttrValue, Rule, Target};
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};

/// Whether a condition matches, and how specific it is: when several match,
/// the one with the most requirements wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Condition {
    pub matches: bool,
    pub specificity: u32,
}

encoded_value!(Condition);

/// A `select()` condition: a `config_setting`, a `constraint_value`, or an
/// alias of one, in a configuration.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ConditionKey {
    pub label: Label,
    pub config: ConfigId,
}

impl Key for ConditionKey {
    type Value = Condition;
    const KIND: &'static str = "CONFIG_CONDITION";

    async fn compute(&self, ctx: &Ctx) -> Result<Condition, SkyError> {
        let package = ctx.compute(&PackageKey(self.label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(self.label.name) else {
            return Err(SkyError::new(format!("{} is not a valid select() condition", self.label)));
        };
        let kind = package.0.rule_class(rule).kind;
        match kind.as_str() {
            "alias" => self.alias(ctx, rule).await,
            "config_setting" => config_setting(ctx, rule, &self.config.get()).await,
            "constraint_value" => {
                let matches = target_platform_has(ctx, &self.config.get(), self.label).await?;
                Ok(Condition { matches, specificity: 1 })
            }
            _ => Err(SkyError::new(format!(
                "{} is a {kind} rule, which isn't a valid select() condition",
                self.label
            ))),
        }
    }
}

impl ConditionKey {
    async fn alias(&self, ctx: &Ctx, rule: &Rule) -> Result<Condition, SkyError> {
        let actual = rule.attr(Symbol::intern("actual")).cloned().unwrap_or(AttrValue::None);
        let AttrValue::Label(actual) = resolve_value(ctx, &actual, self.config).await? else {
            return Err(SkyError::new(format!("{}: alias has no 'actual'", self.label)));
        };
        let key = ConditionKey {
            label: actual,
            config: self.config,
        };
        Ok(*ctx.compute(&key).await?)
    }
}

pub(crate) async fn target_platform_has(ctx: &Ctx, config: &BuildConfiguration, value: Label) -> Result<bool, SkyError> {
    let platform = ctx.compute(&PlatformKey(config.options.platform)).await?;
    let value = ctx.compute(&ConstraintValueKey(value)).await?;
    Ok(platform.has(&value))
}

fn string_dict(rule: &Rule, attr: &str) -> Vec<(String, String)> {
    match rule.attr(Symbol::intern(attr)) {
        Some(AttrValue::StringDict(entries)) => entries.clone(),
        _ => Vec::new(),
    }
}

async fn config_setting(ctx: &Ctx, rule: &Rule, config: &BuildConfiguration) -> Result<Condition, SkyError> {
    let values = string_dict(rule, "values");
    let defines = string_dict(rule, "define_values");
    let flags = match rule.attr(Symbol::intern("flag_values")) {
        Some(AttrValue::LabelKeyedStringDict(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let constraints = labels(rule, "constraint_values");
    let specificity = (values.len() + defines.len() + flags.len() + constraints.len()) as u32;

    let mut matches = values.iter().all(|(option, value)| option_matches(config, option, value));
    matches &= defines.iter().all(|(name, value)| config.define(name) == Some(value.as_str()));
    for (flag, expected) in &flags {
        if !matches {
            break;
        }
        matches &= flag_matches(ctx, config, *flag, expected).await?;
    }
    for constraint in constraints {
        if !matches {
            break;
        }
        matches &= target_platform_has(ctx, config, constraint).await?;
    }
    Ok(Condition { matches, specificity })
}

/// Whether a native option has a value, as `config_setting(values = ...)`
/// asks. `define` matches one `--define`.
fn option_matches(config: &BuildConfiguration, option: &str, expected: &str) -> bool {
    if option == "define" {
        let Some((name, value)) = expected.split_once('=') else {
            return false;
        };
        return config.define(name) == Some(value);
    }
    match config.value(option) {
        Some(actual) => same_option_value(actual, expected),
        None => bazel_config::option_default(option).is_some_and(|default| same_option_value(default, expected)),
    }
}

/// Option values compare as booleans when both are, so `1` matches `true`.
fn same_option_value(actual: &str, expected: &str) -> bool {
    match (parse_bool(actual), parse_bool(expected)) {
        (Some(a), Some(b)) => a == b,
        _ => actual == expected,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "True" | "1" | "yes" => Some(true),
        "false" | "False" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// Whether a Starlark flag has a value: the command line's, or its default.
async fn flag_matches(ctx: &Ctx, config: &BuildConfiguration, flag: Label, expected: &str) -> Result<bool, SkyError> {
    let flag = crate::platform::actual(ctx, flag).await?;
    if let Some((_, value)) = config.options.starlark_flags.iter().find(|(label, _)| *label == flag) {
        return Ok(flag_value_matches(value, expected));
    }
    let package = ctx.compute(&PackageKey(flag.package)).await?;
    let Some(Target::Rule(rule)) = package.0.target(flag.name) else {
        return Err(SkyError::new(format!("{flag} isn't a build setting")));
    };
    // A `config_feature_flag` names its default `default_value`.
    let default = rule
        .attr(Symbol::intern("build_setting_default"))
        .or_else(|| rule.attr(Symbol::intern("default_value")))
        .cloned()
        .unwrap_or(AttrValue::None);
    let default = match default {
        AttrValue::Bool(value) => value.to_string(),
        AttrValue::Int(value) => value.to_string(),
        AttrValue::String(value) => value,
        AttrValue::Label(label) => label.to_string(),
        AttrValue::StringList(values) => values.join(","),
        _ => String::new(),
    };
    Ok(flag_value_matches(&default, expected))
}

fn flag_value_matches(actual: &str, expected: &str) -> bool {
    if same_option_value(actual, expected) {
        return true;
    }
    // Labels compare by what they name, not how they're written.
    let actual = actual.trim_start_matches('@');
    let expected = expected.trim_start_matches('@');
    actual == expected
}
