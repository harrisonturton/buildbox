//! Platforms and the constraints they satisfy.

use bazel_label::Label;
use bazel_loading::PackageKey;
use bazel_packages::{AttrValue, Package, Rule, Target};
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};

/// A platform: the constraint values it has, each with its setting.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Platform {
    pub label: Label,
    /// Constraint values with their settings, sorted by setting.
    pub constraints: Vec<(Label, Label)>,
}

encoded_value!(Platform);

impl Platform {
    pub fn has(&self, value: &ConstraintValue) -> bool {
        match self.constraints.iter().find(|(setting, _)| *setting == value.setting) {
            Some((_, has)) => *has == value.label,
            None => value.is_default,
        }
    }

    pub fn values(&self) -> Vec<Label> {
        self.constraints.iter().map(|(_, value)| *value).collect()
    }
}

/// A `platform` target, with its parents' constraints.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PlatformKey(pub Label);

impl Key for PlatformKey {
    type Value = Platform;
    const KIND: &'static str = "PLATFORM";

    async fn compute(&self, ctx: &Ctx) -> Result<Platform, SkyError> {
        let rule = load_rule(ctx, self.0, "platform").await?;
        let mut constraints = Vec::new();
        for parent in labels(&rule, "parents") {
            let parent = ctx.compute(&PlatformKey(parent)).await?;
            constraints.extend(parent.constraints.iter().copied());
        }
        let values = labels(&rule, "constraint_values");
        let keys: Vec<ConstraintValueKey> = values.iter().map(|value| ConstraintValueKey(*value)).collect();
        for value in ctx.compute_many(&keys).await {
            let value = value?;
            constraints.retain(|(setting, _)| *setting != value.setting);
            constraints.push((value.setting, value.label));
        }
        constraints.sort();
        Ok(Platform {
            label: self.0,
            constraints,
        })
    }
}

/// A `constraint_value` target.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ConstraintValue {
    pub label: Label,
    pub setting: Label,
    /// Whether it's its setting's default, which platforms without a value
    /// for the setting have.
    pub is_default: bool,
}

encoded_value!(ConstraintValue);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ConstraintValueKey(pub Label);

impl Key for ConstraintValueKey {
    type Value = ConstraintValue;
    const KIND: &'static str = "CONSTRAINT_VALUE";

    async fn compute(&self, ctx: &Ctx) -> Result<ConstraintValue, SkyError> {
        let (label, rule) = load_actual_rule(ctx, self.0, "constraint_value").await?;
        let Some(AttrValue::Label(setting)) = rule.attr(Symbol::intern("constraint_setting")).cloned() else {
            return Err(SkyError::new(format!("{}: constraint_value has no constraint_setting", self.0)));
        };
        let setting_rule = load_rule(ctx, setting, "constraint_setting").await?;
        let default = match setting_rule.attr(Symbol::intern("default_constraint_value")) {
            Some(AttrValue::Label(default)) => Some(*default),
            _ => None,
        };
        Ok(ConstraintValue {
            label,
            setting,
            is_default: default == Some(label),
        })
    }
}

/// A target that must be a rule, following aliases, of the kind given.
pub async fn load_rule(ctx: &Ctx, label: Label, kind: &str) -> Result<Rule, SkyError> {
    Ok(load_actual_rule(ctx, label, kind).await?.1)
}

/// The target an alias stands for, following aliases; other targets stand
/// for themselves.
pub async fn actual(ctx: &Ctx, label: Label) -> Result<Label, SkyError> {
    let mut label = label;
    for _ in 0..16 {
        let package = ctx.compute(&PackageKey(label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(label.name) else {
            return Ok(label);
        };
        if package.0.rule_class(rule).kind.as_str() != "alias" {
            return Ok(label);
        }
        let Some(AttrValue::Label(actual)) = rule.attr(Symbol::intern("actual")) else {
            return Ok(label);
        };
        label = *actual;
    }
    Err(SkyError::new(format!("{label}: too many aliases")))
}

/// [`load_rule`], with the label of the rule found.
async fn load_actual_rule(ctx: &Ctx, label: Label, kind: &str) -> Result<(Label, Rule), SkyError> {
    let mut label = label;
    for _ in 0..16 {
        let package = ctx.compute(&PackageKey(label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(label.name) else {
            return Err(missing_rule(&package.0, label, kind));
        };
        let class = package.0.rule_class(rule).kind;
        if class.as_str() == "alias" {
            let Some(AttrValue::Label(actual)) = rule.attr(Symbol::intern("actual")) else {
                return Err(SkyError::new(format!("{label}: alias has no plain 'actual'")));
            };
            label = *actual;
            continue;
        }
        if class.as_str() != kind {
            return Err(SkyError::new(format!("{label} is a {class} rule, not a {kind}")));
        }
        return Ok((label, rule.clone()));
    }
    Err(SkyError::new(format!("{label}: too many aliases")))
}

/// The error for a rule that isn't in its package: the package's own error,
/// if it has one, since that may be why.
fn missing_rule(package: &Package, label: Label, kind: &str) -> SkyError {
    match package.errors.first() {
        Some(error) => SkyError::new(format!("{label}: error loading package: {error}")),
        None => SkyError::new(format!("{label} is not a {kind} rule")),
    }
}

/// The labels of a label list attribute given explicitly.
pub fn labels(rule: &Rule, attr: &str) -> Vec<Label> {
    match rule.attr(Symbol::intern(attr)) {
        Some(AttrValue::LabelList(labels)) => labels.clone(),
        Some(AttrValue::Label(label)) => vec![*label],
        _ => Vec::new(),
    }
}
