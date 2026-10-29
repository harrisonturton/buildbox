//! Whether the conditions of `select()`s match a configuration.

use crate::attrs::resolve_value;
use crate::transition::configured_key;
use bazel_starlark::provider::ProviderInstance;
use crate::platform::{ConstraintValueKey, PlatformKey, labels};
use bazel_config::{BuildConfiguration, ConfigId, FlagValue, OptionError};
use bazel_label::{Label, LabelContext};
use bazel_loading::{PackageKey, RepoMappingKey};
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
            "label_flag" | "label_setting" => self.label_setting(ctx).await,
            "config_setting" => config_setting(ctx, self.label, rule, &self.config.get()).await,
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
    /// A `label_flag` or `label_setting` is the condition its value names,
    /// as it forwards that target's providers.
    async fn label_setting(&self, ctx: &Ctx) -> Result<Condition, SkyError> {
        let Some(setting) = crate::settings::build_setting(ctx, self.label).await? else {
            return Err(SkyError::new(format!("{} is not a valid select() condition", self.label)));
        };
        let config = self.config.get();
        let bazel_config::FlagValue::Label(actual) = setting.value(&config.options) else {
            return Err(SkyError::new(format!("{}: label setting has no label", self.label)));
        };
        let key = ConditionKey {
            label: *actual,
            config: self.config,
        };
        Ok(*ctx.compute(&key).await?)
    }

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
    let platform = ctx.compute(&PlatformKey(config.platform)).await?;
    let value = ctx.compute(&ConstraintValueKey(value)).await?;
    Ok(platform.has(&value))
}

fn string_dict(rule: &Rule, attr: &str) -> Vec<(String, String)> {
    match rule.attr(Symbol::intern(attr)) {
        Some(AttrValue::StringDict(entries)) => entries.clone(),
        _ => Vec::new(),
    }
}

async fn config_setting(ctx: &Ctx, label: Label, rule: &Rule, config: &BuildConfiguration) -> Result<Condition, SkyError> {
    let values = string_dict(rule, "values");
    let defines = string_dict(rule, "define_values");
    let flags = match rule.attr(Symbol::intern("flag_values")) {
        Some(AttrValue::LabelKeyedStringDict(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    let constraints = labels(rule, "constraint_values");
    let specificity = (values.len() + defines.len() + flags.len() + constraints.len()) as u32;

    let mut matches = values_match(ctx, label, config, &values).await?;
    matches &= defines.iter().all(|(name, value)| config.options.define(name) == Some(value.as_str()));
    for (flag, expected) in &flags {
        if !matches {
            break;
        }
        matches &= flag_matches(ctx, label, config, *flag, expected).await?;
    }
    for constraint in constraints {
        if !matches {
            break;
        }
        matches &= target_platform_has(ctx, config, constraint).await?;
    }
    Ok(Condition { matches, specificity })
}

/// Whether the native options have the values `config_setting(values =
/// ...)` gives. Labels in the values are relative to the `config_setting`.
async fn values_match(ctx: &Ctx, label: Label, config: &BuildConfiguration, values: &[(String, String)]) -> Result<bool, SkyError> {
    if values.is_empty() {
        return Ok(true);
    }
    let mapping = ctx.compute(&RepoMappingKey(label.repo())).await?;
    let parse_label = |text: &str| {
        let context = LabelContext::new(label.package, &mapping.0);
        Label::parse(text, &context).map_err(|error| error.to_string())
    };
    for (option, expected) in values {
        let matches = config.options.matches(option, expected, &parse_label).map_err(|error| values_error(label, error))?;
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn values_error(label: Label, error: OptionError) -> SkyError {
    let problem = match error {
        OptionError::Unknown(option) => format!("bb doesn't support matching the option '--{option}' yet"),
        OptionError::Invalid(message) => message,
    };
    SkyError::new(format!("in values attribute of config_setting rule {label}: error while parsing configuration settings: {problem}"))
}

/// Whether a build setting has a value, as `config_setting(flag_values =
/// ...)` asks: the value its `FeatureFlagInfo` gives, if it gives one, or
/// else the value as its type reads it, or for lists, one value the list
/// contains.
async fn flag_matches(ctx: &Ctx, label: Label, config: &BuildConfiguration, flag: Label, expected: &str) -> Result<bool, SkyError> {
    if let Some(value) = feature_flag_info(ctx, flag, config.id).await? {
        return Ok(value == expected);
    }
    let Some(setting) = crate::settings::build_setting(ctx, flag).await? else {
        return feature_flag_matches(ctx, flag, expected).await;
    };
    let mapping = ctx.compute(&RepoMappingKey(label.repo())).await?;
    let parse_label = |text: &str| {
        let context = LabelContext::new(label.package, &mapping.0);
        Label::parse(text, &context).map_err(|error| error.to_string())
    };
    let error = |problem: String| SkyError::new(format!("in flag_values attribute of config_setting rule {label}: {problem}"));
    let Ok(wanted) = FlagValue::parse(expected, &setting.default, setting.spec.multiple, &parse_label) else {
        return Err(error(format!(
            "error while parsing user-defined configuration values: '{expected}' cannot be converted to {flag} type {}",
            setting.default.type_name()
        )));
    };
    match (setting.value(&config.options), wanted) {
        (FlagValue::Strings(actual), FlagValue::Strings(wanted)) => {
            let [wanted] = wanted.as_slice() else {
                return Err(error(format!(
                    "\"{expected}\" not a valid value for flag {flag}. Only single, exact values are allowed. If you want to match multiple values, consider Skylib's selects.config_setting_group"
                )));
            };
            Ok(actual.contains(wanted))
        }
        (actual, wanted) => Ok(*actual == wanted),
    }
}

/// The value a flag's `FeatureFlagInfo` gives in a configuration, if the
/// flag is a rule that returns one.
async fn feature_flag_info(ctx: &Ctx, flag: Label, config: ConfigId) -> Result<Option<String>, SkyError> {
    let package = ctx.compute(&PackageKey(flag.package)).await?;
    if !matches!(package.0.target(flag.name), Some(Target::Rule(_))) {
        return Ok(None);
    }
    let key = configured_key(ctx, flag, config).await?;
    let target = ctx.compute(&key).await?;
    let providers = target.providers()?;
    let Some(info) = providers.get().and_then(|value| value.provider("FeatureFlagInfo")).and_then(ProviderInstance::from_value) else {
        return Ok(None);
    };
    if let Some(error) = info.field("error").and_then(|error| error.unpack_str()) {
        return Err(SkyError::new(error.to_string()));
    }
    let value = info.field("value").and_then(|value| value.unpack_str()).unwrap_or_default();
    Ok(Some(value.to_string()))
}

/// Whether a `config_feature_flag`, which bb only ever has at its default,
/// has a value.
async fn feature_flag_matches(ctx: &Ctx, flag: Label, expected: &str) -> Result<bool, SkyError> {
    let flag = crate::platform::actual(ctx, flag).await?;
    let package = ctx.compute(&PackageKey(flag.package)).await?;
    let Some(Target::Rule(rule)) = package.0.target(flag.name) else {
        return Err(SkyError::new(format!("{flag} isn't a build setting")));
    };
    match rule.attr(Symbol::intern("default_value")) {
        Some(AttrValue::String(default)) => Ok(default == expected),
        _ => Err(SkyError::new(format!("{flag} isn't a build setting"))),
    }
}
