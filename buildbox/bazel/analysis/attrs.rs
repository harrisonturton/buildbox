//! Attribute values in a configuration: defaults filled in, and
//! `select()`s resolved.

use crate::condition::ConditionKey;
use crate::configured::ConfiguredTargetKey;
use crate::late_bound::late_bound_value;
use bazel_config::{BuildOptions, ConfigId};
use bazel_label::Label;
use bazel_loading::PackageKey;
use bazel_packages::{AttrDefault, AttrSchema, AttrValue, DEFAULT_CONDITION, Rule, RuleClass, Select, SelectorPart, Target};
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};

/// A rule's attributes in a configuration, as `cquery` prints them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ConfiguredAttrsKey(pub ConfiguredTargetKey);

/// The value of every attribute of a rule in a configuration, in its
/// class's order; none for a target that isn't a rule.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ConfiguredAttrs(pub Vec<(Symbol, AttrValue)>);

encoded_value!(ConfiguredAttrs);

impl Key for ConfiguredAttrsKey {
    type Value = ConfiguredAttrs;
    const KIND: &'static str = "CONFIGURED_ATTRIBUTES";

    async fn compute(&self, ctx: &Ctx) -> Result<ConfiguredAttrs, SkyError> {
        let label = self.0.label;
        let package = ctx.compute(&PackageKey(label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(label.name) else {
            return Ok(ConfiguredAttrs(Vec::new()));
        };
        let class = bazel_loading::rule_class(ctx, package.0.rule_class(rule)).await?;
        let config = self.0.config.get().without_aspects();
        Ok(ConfiguredAttrs(resolve_attrs(ctx, rule, &class, config).await?))
    }
}

/// The value of every attribute of a rule in a configuration, in its
/// class's order.
pub async fn resolve_attrs(ctx: &Ctx, rule: &Rule, class: &RuleClass, config: ConfigId) -> Result<Vec<(Symbol, AttrValue)>, SkyError> {
    let mut values = Vec::with_capacity(class.attrs.len());
    let configuration = config.get();
    for schema in &class.attrs {
        let raw = given_or_default(rule, schema, &configuration.options).map_err(|error| error.context(format_args!("attribute '{}'", schema.name)))?;
        let value = resolve_value(ctx, &raw, config).await.map_err(|error| error.context(format_args!("attribute '{}'", schema.name)))?;
        values.push((schema.name, value));
    }
    Ok(values)
}

/// An attribute's value as the rule gives it, or as its default computes
/// under `options`. `name` is the rule's name, which isn't kept among its
/// attributes.
fn given_or_default(rule: &Rule, schema: &AttrSchema, options: &BuildOptions) -> Result<AttrValue, SkyError> {
    if schema.name.as_str() == "name" {
        return Ok(AttrValue::String(rule.name.as_str().to_string()));
    }
    if let Some(value) = rule.attr_or_computed(schema.name) {
        return Ok(value.clone());
    }
    let value = match &schema.default {
        AttrDefault::Value(value) => value.clone(),
        AttrDefault::Computed => AttrValue::default_for(schema.ty),
        AttrDefault::LateBound(late_bound) => late_bound_value(late_bound, options)?,
    };
    Ok(value)
}

/// A value with its `select()`s resolved in a configuration.
pub async fn resolve_value(ctx: &Ctx, value: &AttrValue, config: ConfigId) -> Result<AttrValue, SkyError> {
    let AttrValue::Configurable(parts) = value else {
        return Ok(value.clone());
    };
    let mut resolved = Vec::with_capacity(parts.len());
    for part in parts {
        let value = match part {
            SelectorPart::Value(value) => value.clone(),
            SelectorPart::Select(select) => choose(ctx, select, config).await?,
        };
        resolved.push(value);
    }
    concat(resolved)
}

/// The value of the branch of a `select()` the configuration matches.
async fn choose(ctx: &Ctx, select: &Select, config: ConfigId) -> Result<AttrValue, SkyError> {
    let conditions: Vec<Label> = select
        .branches
        .iter()
        .map(|(condition, _)| *condition)
        .filter(|condition| !is_default(*condition))
        .collect();
    let keys: Vec<ConditionKey> = conditions.iter().map(|label| ConditionKey { label: *label, config }).collect();
    let results = ctx.compute_many(&keys).await;

    let mut best: Option<(Label, u32)> = None;
    let mut ambiguous = None;
    for (label, result) in conditions.iter().zip(results) {
        let condition = result?;
        if !condition.matches {
            continue;
        }
        match best {
            Some((_, specificity)) if specificity > condition.specificity => {}
            Some((other, specificity)) if specificity == condition.specificity && other != *label => ambiguous = Some((other, *label)),
            _ => {
                best = Some((*label, condition.specificity));
                ambiguous = None;
            }
        }
    }
    if let Some((a, b)) = ambiguous {
        return Err(SkyError::new(format!("illegal ambiguous match on configurable attribute: {a} and {b} both match")));
    }
    let chosen = match best {
        Some((label, _)) => select.branches.iter().find(|(condition, _)| *condition == label),
        None => select.branches.iter().find(|(condition, _)| is_default(*condition)),
    };
    match chosen {
        Some((_, value)) => Ok(value.clone()),
        None => Err(no_match(select, &conditions)),
    }
}

fn no_match(select: &Select, conditions: &[Label]) -> SkyError {
    if !select.no_match_error.is_empty() {
        return SkyError::new(select.no_match_error.clone());
    }
    let listed: Vec<String> = conditions.iter().map(|label| format!("  {label}")).collect();
    SkyError::new(format!(
        "configurable attribute doesn't match this configuration; none of these conditions match:\n{}",
        listed.join("\n")
    ))
}

/// Whether a condition is [`DEFAULT_CONDITION`], in whichever repository
/// it's written.
fn is_default(condition: Label) -> bool {
    let (package, name) = DEFAULT_CONDITION.trim_start_matches("//").split_once(':').unwrap_or_default();
    condition.package.path.to_string() == package && condition.name.as_str() == name
}

/// The values of a configurable attribute's parts, joined with `+`.
fn concat(values: Vec<AttrValue>) -> Result<AttrValue, SkyError> {
    let mut values = values.into_iter();
    let Some(mut joined) = values.next() else {
        return Ok(AttrValue::None);
    };
    for value in values {
        joined = concat_two(joined, value)?;
    }
    Ok(joined)
}

fn concat_two(a: AttrValue, b: AttrValue) -> Result<AttrValue, SkyError> {
    Ok(match (a, b) {
        (AttrValue::StringList(mut a), AttrValue::StringList(b)) => {
            a.extend(b);
            AttrValue::StringList(a)
        }
        (AttrValue::LabelList(mut a), AttrValue::LabelList(b)) => {
            a.extend(b);
            AttrValue::LabelList(a)
        }
        (AttrValue::IntList(mut a), AttrValue::IntList(b)) => {
            a.extend(b);
            AttrValue::IntList(a)
        }
        (AttrValue::String(a), AttrValue::String(b)) => AttrValue::String(a + &b),
        (AttrValue::StringDict(mut a), AttrValue::StringDict(b)) => {
            a.extend(b);
            AttrValue::StringDict(a)
        }
        (AttrValue::StringListDict(mut a), AttrValue::StringListDict(b)) => {
            a.extend(b);
            AttrValue::StringListDict(a)
        }
        (AttrValue::LabelKeyedStringDict(mut a), AttrValue::LabelKeyedStringDict(b)) => {
            a.extend(b);
            AttrValue::LabelKeyedStringDict(a)
        }
        (AttrValue::None, b) => b,
        (a, AttrValue::None) => a,
        (a, b) => return Err(SkyError::new(format!("can't concatenate {a:?} and {b:?} in a configurable attribute"))),
    })
}
