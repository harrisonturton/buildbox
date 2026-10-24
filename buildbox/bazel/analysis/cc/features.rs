//! C++ toolchain features: which are enabled for a rule, and the command
//! lines and environments they give actions, as Bazel's
//! `CcToolchainFeatures` computes them.

use skycodec::{Decode, Encode};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The features and action configs of a toolchain, in its order.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ToolchainFeatures {
    pub features: Vec<Feature>,
    pub action_configs: Vec<ActionConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Feature {
    pub name: String,
    pub enabled: bool,
    pub flag_sets: Vec<FlagSet>,
    pub env_sets: Vec<EnvSet>,
    /// Sets of features, one of which must be enabled for this one to be.
    pub requires: Vec<Vec<String>>,
    pub implies: Vec<String>,
    pub provides: Vec<String>,
}

/// How an action is run: its tools, and flags of its own.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ActionConfig {
    pub action_name: String,
    pub enabled: bool,
    pub tools: Vec<Tool>,
    pub flag_sets: Vec<FlagSet>,
    pub implies: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Tool {
    /// The tool's path: absolute, or relative to the execution root.
    pub path: String,
    pub with_features: Vec<WithFeatureSet>,
    pub execution_requirements: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct FlagSet {
    /// The actions it applies to; for an action config's, its own.
    pub actions: Vec<String>,
    pub with_features: Vec<WithFeatureSet>,
    pub expand_if_all_available: Vec<String>,
    pub flag_groups: Vec<FlagGroup>,
}

/// Features that must be enabled, and features that must not be.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct WithFeatureSet {
    pub features: Vec<String>,
    pub not_features: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct FlagGroup {
    pub flags: Vec<String>,
    pub flag_groups: Vec<FlagGroup>,
    pub iterate_over: Option<String>,
    pub expand_if_available: Option<String>,
    pub expand_if_not_available: Option<String>,
    pub expand_if_true: Option<String>,
    pub expand_if_false: Option<String>,
    pub expand_if_equal: Option<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct EnvSet {
    pub actions: Vec<String>,
    pub with_features: Vec<WithFeatureSet>,
    pub env_entries: Vec<EnvEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct EnvEntry {
    pub key: String,
    pub value: String,
    pub expand_if_available: Option<String>,
}

/// The value of a build variable, which flags expand.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Variable {
    String(String),
    Bool(bool),
    Sequence(Vec<Variable>),
    Structure(BTreeMap<String, Variable>),
}

/// Build variables, by name.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct Variables {
    pub values: BTreeMap<String, Variable>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FeatureError(pub String);

/// Which features are enabled for a rule.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct FeatureConfiguration {
    pub toolchain: Arc<ToolchainFeatures>,
    pub enabled: BTreeSet<String>,
}

/// The actions Bazel requests the action configs of, as features, so that
/// a toolchain's configs for them are enabled.
const ACTION_NAMES: [&str; 31] = [
    "c-compile",
    "c++-compile",
    "c++-header-parsing",
    "c++-module-compile",
    "c++-module-codegen",
    "c++-module-deps-scanning",
    "c++20-module-compile",
    "c++20-module-codegen",
    "linkstamp-compile",
    "lto-backend",
    "lto-indexing",
    "lto-index-for-executable",
    "lto-index-for-dynamic-library",
    "lto-index-for-nodeps-dynamic-library",
    "clif-match",
    "assemble",
    "preprocess-assemble",
    "c++-link-executable",
    "c++-link-dynamic-library",
    "c++-link-nodeps-dynamic-library",
    "c++-link-static-library",
    "strip",
    "objcopy_embed_data",
    "validate-static-library",
    "llvm-cov",
    "llvm-profdata",
    "objc-compile",
    "objc++-compile",
    "objc-executable",
    "objc-fully-link",
    "cc-flags-make-variable",
];

impl ToolchainFeatures {
    fn feature(&self, name: &str) -> Option<&Feature> {
        self.features.iter().find(|feature| feature.name == name)
    }

    fn action_config(&self, name: &str) -> Option<&ActionConfig> {
        self.action_configs.iter().find(|config| config.action_name == name)
    }

    fn is_defined(&self, name: &str) -> bool {
        self.feature(name).is_some() || self.action_config(name).is_some()
    }

    fn implies(&self, name: &str) -> &[String] {
        if let Some(feature) = self.feature(name) {
            return &feature.implies;
        }
        self.action_config(name).map(|config| config.implies.as_slice()).unwrap_or_default()
    }

    /// Choose the enabled features: those enabled by default, the action
    /// configs of Bazel's actions, and those requested, with what they
    /// imply, less the unsupported ones and those whose requirements aren't
    /// met.
    pub fn configure(self: &Arc<Self>, requested: &[String], unsupported: &[String]) -> Result<FeatureConfiguration, FeatureError> {
        let unsupported: BTreeSet<&str> = unsupported.iter().map(String::as_str).collect();
        let mut wanted: Vec<&str> = self.features.iter().filter(|feature| feature.enabled).map(|feature| feature.name.as_str()).collect();
        wanted.extend(self.action_configs.iter().filter(|config| config.enabled).map(|config| config.action_name.as_str()));
        wanted.extend(ACTION_NAMES);
        wanted.extend(requested.iter().map(String::as_str));

        let mut enabled = BTreeSet::new();
        for name in wanted {
            self.enable(name, &unsupported, &mut enabled);
        }
        while let Some(unsatisfied) = enabled.iter().find(|name| !self.is_satisfied(name, &enabled)).cloned() {
            enabled.remove(&unsatisfied);
        }
        self.check_provides(&enabled)?;
        Ok(FeatureConfiguration {
            toolchain: self.clone(),
            enabled,
        })
    }

    fn enable(&self, name: &str, unsupported: &BTreeSet<&str>, enabled: &mut BTreeSet<String>) {
        if unsupported.contains(name) || !self.is_defined(name) || !enabled.insert(name.to_string()) {
            return;
        }
        for implied in self.implies(name) {
            self.enable(implied, unsupported, enabled);
        }
    }

    /// Whether a feature can stay enabled: one of its sets of required
    /// features is enabled, and so is everything it implies that exists.
    fn is_satisfied(&self, name: &str, enabled: &BTreeSet<String>) -> bool {
        let implied_enabled = self.implies(name).iter().all(|implied| !self.is_defined(implied) || enabled.contains(implied));
        let Some(feature) = self.feature(name) else {
            return implied_enabled;
        };
        let requirements_met = feature.requires.is_empty() || feature.requires.iter().any(|set| set.iter().all(|required| enabled.contains(required)));
        implied_enabled && requirements_met
    }

    fn check_provides(&self, enabled: &BTreeSet<String>) -> Result<(), FeatureError> {
        let mut provided: BTreeMap<&str, &str> = BTreeMap::new();
        for feature in self.features.iter().filter(|feature| enabled.contains(&feature.name)) {
            for capability in &feature.provides {
                if let Some(other) = provided.insert(capability, &feature.name) {
                    return Err(FeatureError(format!(
                        "symbol '{capability}' is provided by all of the following features: {other} {}",
                        feature.name
                    )));
                }
            }
        }
        Ok(())
    }
}

impl FeatureConfiguration {
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    fn with_features_satisfied(&self, sets: &[WithFeatureSet]) -> bool {
        sets.is_empty()
            || sets.iter().any(|set| {
                set.features.iter().all(|feature| self.is_enabled(feature)) && set.not_features.iter().all(|feature| !self.is_enabled(feature))
            })
    }

    /// The flags an action gets: those of its action config, then those of
    /// each enabled feature, in the toolchain's order.
    pub fn command_line(&self, action: &str, variables: &Variables) -> Result<Vec<String>, FeatureError> {
        let mut out = Vec::new();
        if let Some(config) = self.toolchain.action_config(action).filter(|config| self.is_enabled(&config.action_name)) {
            for flag_set in &config.flag_sets {
                self.expand_flag_set(flag_set, None, variables, &mut out)?;
            }
        }
        for feature in self.toolchain.features.iter().filter(|feature| self.is_enabled(&feature.name)) {
            for flag_set in &feature.flag_sets {
                self.expand_flag_set(flag_set, Some(action), variables, &mut out)?;
            }
        }
        Ok(out)
    }

    fn expand_flag_set(&self, flag_set: &FlagSet, action: Option<&str>, variables: &Variables, out: &mut Vec<String>) -> Result<(), FeatureError> {
        if let Some(action) = action {
            if !flag_set.actions.iter().any(|candidate| candidate == action) {
                return Ok(());
            }
        }
        if !self.with_features_satisfied(&flag_set.with_features) {
            return Ok(());
        }
        let scope = Scope::new(variables);
        if !flag_set.expand_if_all_available.iter().all(|name| scope.lookup(name).is_some()) {
            return Ok(());
        }
        for group in &flag_set.flag_groups {
            expand_group(group, &scope, out)?;
        }
        Ok(())
    }

    /// The environment an action gets from the enabled features.
    pub fn environment(&self, action: &str, variables: &Variables) -> Result<Vec<(String, String)>, FeatureError> {
        let scope = Scope::new(variables);
        let mut env = Vec::new();
        for feature in self.toolchain.features.iter().filter(|feature| self.is_enabled(&feature.name)) {
            for env_set in &feature.env_sets {
                if !env_set.actions.iter().any(|candidate| candidate == action) || !self.with_features_satisfied(&env_set.with_features) {
                    continue;
                }
                for entry in &env_set.env_entries {
                    if entry.expand_if_available.as_deref().is_some_and(|name| scope.lookup(name).is_none()) {
                        continue;
                    }
                    env.push((entry.key.clone(), expand_template(&entry.value, &scope)?));
                }
            }
        }
        Ok(env)
    }

    /// The tool an action runs: the first of its action config's tools
    /// whose features are enabled.
    pub fn tool(&self, action: &str) -> Result<&Tool, FeatureError> {
        let config = self
            .toolchain
            .action_config(action)
            .filter(|config| self.is_enabled(&config.action_name))
            .ok_or_else(|| FeatureError(format!("action '{action}' isn't configured by the C++ toolchain")))?;
        config
            .tools
            .iter()
            .find(|tool| self.with_features_satisfied(&tool.with_features))
            .ok_or_else(|| FeatureError(format!("no tool of the C++ toolchain matches the enabled features for action '{action}'")))
    }

    pub fn action_is_configured(&self, action: &str) -> bool {
        self.toolchain.action_config(action).is_some_and(|config| self.is_enabled(&config.action_name))
    }
}

/// Variables in scope while expanding a flag group: the build variables,
/// and those `iterate_over` binds.
struct Scope<'a> {
    variables: &'a Variables,
    bound: Vec<(&'a str, &'a Variable)>,
}

impl<'a> Scope<'a> {
    fn new(variables: &'a Variables) -> Scope<'a> {
        Scope {
            variables,
            bound: Vec::new(),
        }
    }

    fn with(&self, name: &'a str, value: &'a Variable) -> Scope<'a> {
        let mut bound = self.bound.clone();
        bound.push((name, value));
        Scope {
            variables: self.variables,
            bound,
        }
    }

    /// A variable's value, following dots into structures, as in
    /// `libraries_to_link.name`.
    fn lookup(&self, name: &str) -> Option<&'a Variable> {
        let mut parts = name.split('.');
        let first = parts.next()?;
        let mut value = self
            .bound
            .iter()
            .rev()
            .find(|(bound, _)| *bound == first)
            .map(|(_, value)| *value)
            .or_else(|| self.variables.values.get(first))?;
        for part in parts {
            let Variable::Structure(fields) = value else {
                return None;
            };
            value = fields.get(part)?;
        }
        Some(value)
    }
}

fn is_true(value: &Variable) -> bool {
    match value {
        Variable::Bool(value) => *value,
        Variable::String(value) => !value.is_empty() && value != "0" && value != "false",
        Variable::Sequence(values) => !values.is_empty(),
        Variable::Structure(_) => true,
    }
}

fn can_expand(group: &FlagGroup, scope: &Scope<'_>) -> bool {
    if group.expand_if_available.as_deref().is_some_and(|name| scope.lookup(name).is_none()) {
        return false;
    }
    if group.expand_if_not_available.as_deref().is_some_and(|name| scope.lookup(name).is_some()) {
        return false;
    }
    if group.expand_if_true.as_deref().is_some_and(|name| !scope.lookup(name).is_some_and(is_true)) {
        return false;
    }
    if group.expand_if_false.as_deref().is_some_and(|name| scope.lookup(name).is_none_or(is_true)) {
        return false;
    }
    if let Some((name, expected)) = &group.expand_if_equal {
        return matches!(scope.lookup(name), Some(Variable::String(value)) if value == expected);
    }
    true
}

fn expand_group(group: &FlagGroup, scope: &Scope<'_>, out: &mut Vec<String>) -> Result<(), FeatureError> {
    if !can_expand(group, scope) {
        return Ok(());
    }
    let Some(name) = &group.iterate_over else {
        return expand_group_body(group, scope, out);
    };
    let Some(Variable::Sequence(values)) = scope.lookup(name) else {
        return match scope.lookup(name) {
            None => Ok(()),
            Some(_) => Err(FeatureError(format!("cannot iterate over '{name}': it isn't a sequence"))),
        };
    };
    for value in values {
        expand_group_body(group, &scope.with(name, value), out)?;
    }
    Ok(())
}

fn expand_group_body(group: &FlagGroup, scope: &Scope<'_>, out: &mut Vec<String>) -> Result<(), FeatureError> {
    for flag in &group.flags {
        out.push(expand_template(flag, scope)?);
    }
    for child in &group.flag_groups {
        expand_group(child, scope, out)?;
    }
    Ok(())
}

/// A flag with its `%{variable}`s substituted, and `%%` as `%`.
fn expand_template(template: &str, scope: &Scope<'_>) -> Result<String, FeatureError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(index) = rest.find('%') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        if let Some(remaining) = after.strip_prefix('%') {
            out.push('%');
            rest = remaining;
            continue;
        }
        let Some(inner) = after.strip_prefix('{') else {
            return Err(FeatureError(format!("invalid flag '{template}': '%' must start '%{{variable}}' or be doubled")));
        };
        let Some(end) = inner.find('}') else {
            return Err(FeatureError(format!("invalid flag '{template}': unterminated variable")));
        };
        let name = &inner[..end];
        match scope.lookup(name) {
            Some(Variable::String(value)) => out.push_str(value),
            Some(Variable::Bool(value)) => out.push_str(if *value { "1" } else { "0" }),
            Some(_) => return Err(FeatureError(format!("cannot expand variable '{name}' in '{template}': it isn't a string"))),
            None => return Err(FeatureError(format!("cannot expand variable '{name}' in '{template}': it isn't defined"))),
        }
        rest = &inner[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Variables {
    pub fn set(&mut self, name: &str, value: Variable) {
        self.values.insert(name.to_string(), value);
    }

    pub fn string(&mut self, name: &str, value: impl Into<String>) {
        self.set(name, Variable::String(value.into()));
    }

    pub fn strings(&mut self, name: &str, values: impl IntoIterator<Item = String>) {
        self.set(name, Variable::Sequence(values.into_iter().map(Variable::String).collect()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(flags: &[&str]) -> FlagGroup {
        FlagGroup {
            flags: flags.iter().map(|flag| flag.to_string()).collect(),
            ..Default::default()
        }
    }

    fn feature(name: &str, enabled: bool, groups: Vec<FlagGroup>) -> Feature {
        Feature {
            name: name.to_string(),
            enabled,
            flag_sets: vec![FlagSet {
                actions: vec!["c-compile".to_string()],
                flag_groups: groups,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn expands_enabled_features_in_order() {
        let mut defines = flags(&["-D%{preprocessor_defines}"]);
        defines.iterate_over = Some("preprocessor_defines".to_string());
        let mut optional = flags(&["-o", "%{output_file}"]);
        optional.expand_if_available = Some("output_file".to_string());
        let toolchain = Arc::new(ToolchainFeatures {
            features: vec![
                feature("defaults", true, vec![flags(&["-Wall"])]),
                feature("defines", true, vec![defines]),
                feature("disabled", false, vec![flags(&["-never"])]),
                feature("output", true, vec![optional]),
            ],
            action_configs: Vec::new(),
        });
        let configuration = toolchain.configure(&[], &[]).unwrap();
        let mut variables = Variables::default();
        variables.strings("preprocessor_defines", ["A=1".to_string(), "B".to_string()]);
        let command = configuration.command_line("c-compile", &variables).unwrap();
        assert_eq!(command, ["-Wall", "-DA=1", "-DB"]);
        assert!(configuration.command_line("c++-link-executable", &variables).unwrap().is_empty());
    }

    #[test]
    fn requirements_and_implications_decide_what_is_enabled() {
        let mut needs_opt = feature("needs_opt", true, Vec::new());
        needs_opt.requires = vec![vec!["opt".to_string()]];
        let mut implies = feature("implies_missing", false, Vec::new());
        implies.implies = vec!["unsupported".to_string()];
        let toolchain = Arc::new(ToolchainFeatures {
            features: vec![
                feature("opt", false, Vec::new()),
                needs_opt,
                implies,
                feature("unsupported", false, Vec::new()),
            ],
            action_configs: Vec::new(),
        });
        let plain = toolchain.configure(&[], &[]).unwrap();
        assert!(!plain.is_enabled("needs_opt"));
        let opt = toolchain.configure(&["opt".to_string()], &[]).unwrap();
        assert!(opt.is_enabled("needs_opt"));
        let blocked = toolchain.configure(&["implies_missing".to_string()], &["unsupported".to_string()]).unwrap();
        assert!(!blocked.is_enabled("implies_missing"));
    }

    #[test]
    fn templates_reach_into_structures_and_escape_percent() {
        let mut library = BTreeMap::new();
        library.insert("name".to_string(), Variable::String("libfoo.a".to_string()));
        library.insert("type".to_string(), Variable::String("static_library".to_string()));
        let mut variables = Variables::default();
        variables.set("libraries_to_link", Variable::Sequence(vec![Variable::Structure(library)]));
        let mut inner = flags(&["%{libraries_to_link.name}", "100%%"]);
        inner.expand_if_equal = Some(("libraries_to_link.type".to_string(), "static_library".to_string()));
        let group = FlagGroup {
            iterate_over: Some("libraries_to_link".to_string()),
            flag_groups: vec![inner],
            ..Default::default()
        };
        let toolchain = Arc::new(ToolchainFeatures {
            features: vec![feature("libraries", true, vec![group])],
            action_configs: Vec::new(),
        });
        let configuration = toolchain.configure(&[], &[]).unwrap();
        assert_eq!(configuration.command_line("c-compile", &variables).unwrap(), ["libfoo.a", "100%"]);
    }
}
