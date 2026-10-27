//! The checks Bazel makes of a rule's direct dependencies before it
//! analyzes the rule: that each is a kind of file or rule its attribute
//! allows, with the providers the attribute requires, that it's visible to
//! the rule, testonly only if the rule is, and warnings for deprecated ones.

use crate::artifact::Artifact;
use crate::build_ctx::Dep;
use crate::configured::{ConfiguredTarget, ConfiguredTargetKey, RuleSetup};
use crate::package_spec::PackageSpecification;
use crate::transition::dep_configs;
use bazel_config::ConfigId;
use bazel_fs::{FileState, FileStateKey, RootedPath};
use bazel_label::{Label, PackageId};
use bazel_loading::{PackageKey, RepoRootKey};
use bazel_packages::{AttrDefault, AttrSchema, AttrValue, DepRequirements, FileTypes, Package, Rule, RuleClass, Target};
use intern::Symbol;
use skyframe::{Ctx, SkyError};
use std::collections::BTreeMap;
use starlark::values::ValueLike;
use std::sync::Arc;

/// What checking a rule's dependencies found: errors, which fail its
/// analysis, and warnings, each as Bazel words it after the location.
#[derive(Default)]
pub(crate) struct Checked {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Check the dependencies of `rule`, as Bazel's `RuleContext` and its
/// prerequisite validator do, in the order they report.
pub(crate) async fn check(
    ctx: &Ctx,
    key: ConfiguredTargetKey,
    package: &Package,
    rule: &Rule,
    setup: &RuleSetup,
    deps: &BTreeMap<(Label, ConfigId), Dep>,
) -> Result<Checked, SkyError> {
    let mut consumer = Consumer::of(key.label, package, rule, &setup.class, &setup.attrs);
    consumer.check_visibility = key.config.get().options.bool("check_visibility");
    let mut checked = Checked::default();
    for (schema, (_, value)) in setup.class.attrs.iter().zip(&setup.attrs) {
        if !schema.is_dependency || !schema.ty.has_labels() || schema.ty.is_output() || schema.name.as_str().starts_with(':') {
            continue;
        }
        let mut labels = Vec::new();
        value.labels(&mut labels);
        for (_, config) in dep_configs(schema.cfg, schema.name, setup.deps_config, setup.exec_config, &setup.attr_configs) {
            for label in &labels {
                let Some(dep) = deps.get(&(*label, config)) else {
                    continue;
                };
                let found = Found::of(ctx, *label, config, dep).await?;
                check_dep(ctx, &consumer, schema, &found, &mut checked).await?;
            }
        }
    }
    check_non_empty(&consumer, &setup.class, &setup.attrs, &mut checked);
    check_conditions(ctx, &consumer, rule, &mut checked).await?;
    Ok(checked)
}

/// The rule whose dependencies are checked.
struct Consumer<'a> {
    label: Label,
    kind: Symbol,
    rule: &'a Rule,
    class: &'a RuleClass,
    /// Whether it may depend on testonly targets.
    testonly: bool,
    /// Whether it says it's deprecated, which silences warnings for its
    /// deprecated dependencies.
    deprecated: bool,
    /// The `.bzl` file defining a Starlark rule, which its implicit
    /// dependencies must be visible to.
    definition: Option<Label>,
    /// Whether `--check_visibility` asks for visibility to be checked.
    check_visibility: bool,
}

impl<'a> Consumer<'a> {
    fn of(label: Label, package: &Package, rule: &'a Rule, class: &'a RuleClass, attrs: &[(Symbol, AttrValue)]) -> Consumer<'a> {
        let attr = |name: &str| attrs.iter().find(|(attr, _)| attr.as_str() == name).map(|(_, value)| value);
        let testonly = match attr("testonly") {
            Some(AttrValue::Bool(testonly)) => *testonly,
            _ => package.args.default_testonly,
        };
        Consumer {
            label,
            kind: class.kind,
            rule,
            class,
            testonly: testonly || class.test,
            deprecated: rule.attr(Symbol::intern("deprecation")).is_some(),
            definition: package.rule_class(rule).definition.map(|(bzl, _)| bzl),
            check_visibility: true,
        }
    }

    /// An attribute error, as Bazel words it.
    fn attribute_error(&self, schema: &AttrSchema, message: &str) -> String {
        format!("in {} attribute of {} rule {}: {message}", display_name(schema), self.kind, self.label)
    }

    /// A rule error, as Bazel words it.
    fn rule_error(&self, message: &str) -> String {
        format!("in {} rule {}: {message}", self.kind, self.label)
    }
}

/// An attribute's name as Bazel's errors give it: a Starlark rule's
/// private attributes start with `$`, or `:` if the configuration chooses
/// their default.
pub(crate) fn display_name(schema: &AttrSchema) -> String {
    let name = schema.name.as_str();
    let Some(rest) = name.strip_prefix('_') else {
        return name.to_string();
    };
    match schema.default {
        AttrDefault::LateBound(_) => format!(":{rest}"),
        _ => format!("${rest}"),
    }
}

/// A dependency, as the checks see it.
struct Found {
    /// The aliases from the attribute's label to the target, if it names
    /// one: the alias's own visibility counts, not its target's.
    aliases: Vec<Label>,
    /// The label of the target itself.
    label: Label,
    what: What,
    /// What the target is, as `<kind> '<label>'` names it.
    kind: String,
    /// Its `DefaultInfo` files.
    files: Vec<Artifact>,
    /// The keys of its providers, with those of aspects applied to it.
    providers: Vec<String>,
    /// The target the attribute names, which gives the visibility: the
    /// first alias, or the target.
    visible_as: Label,
}

/// What kind of target a dependency is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum What {
    SourceFile,
    GeneratedFile,
    PackageGroup,
    Rule,
}

impl Found {
    async fn of(ctx: &Ctx, label: Label, config: ConfigId, dep: &Dep) -> Result<Found, SkyError> {
        let (target, applied): (Option<&Arc<ConfiguredTarget>>, &[Arc<ConfiguredTarget>]) = match dep {
            Dep::File(_, _) => (None, &[]),
            Dep::Target(target) => (Some(target), &[]),
            Dep::Aspect(applied) => (applied.first(), applied.get(1..).unwrap_or(&[])),
        };
        let (aliases, actual) = match target {
            Some(target) => alias_chain(ctx, label, config, target).await?,
            None => (Vec::new(), label),
        };
        let (what, kind) = match dep {
            Dep::File(_, artifact) if artifact.is_source() => (What::SourceFile, "source file".to_string()),
            Dep::File(_, _) => (What::GeneratedFile, "generated file".to_string()),
            _ => match target.map(|target| target.kind.as_str()) {
                Some(crate::configured::PACKAGE_GROUP) => (What::PackageGroup, "package group".to_string()),
                Some("source file") => (What::SourceFile, "source file".to_string()),
                Some("generated file") => (What::GeneratedFile, "generated file".to_string()),
                Some(kind) => (What::Rule, format!("{kind} rule")),
                None => (What::Rule, "rule".to_string()),
            },
        };
        let (files, mut providers) = match (dep, target) {
            (Dep::File(_, artifact), _) => (vec![*artifact], Vec::new()),
            (_, Some(target)) => provided(target)?,
            _ => (Vec::new(), Vec::new()),
        };
        for application in applied {
            providers.extend(provided(application)?.1);
        }
        Ok(Found {
            visible_as: aliases.first().copied().unwrap_or(actual),
            aliases,
            label: actual,
            what,
            kind,
            files,
            providers,
        })
    }

    fn is_file(&self) -> bool {
        matches!(self.what, What::SourceFile | What::GeneratedFile)
    }

    /// The target as Bazel's errors describe it, through the aliases that
    /// name it, by its kind or as `target`.
    fn describe(&self, with_kind: bool) -> String {
        let kind = if with_kind { self.kind.as_str() } else { "target" };
        let Some((first, rest)) = self.aliases.split_first() else {
            return format!("{kind} '{}'", self.label);
        };
        let mut described = format!("alias '{first}' referring to {kind} '{}'", self.label);
        if !rest.is_empty() {
            let through: Vec<String> = rest.iter().map(Label::to_string).collect();
            described.push_str(&format!(" through '{}'", through.join("' -> '")));
        }
        described
    }
}

/// The `DefaultInfo` files and provider keys of a configured target.
fn provided(target: &ConfiguredTarget) -> Result<(Vec<Artifact>, Vec<String>), SkyError> {
    let providers = target.providers()?;
    let Some(value) = providers.get() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let files = crate::actions::files(value.default_info("files"), "files").unwrap_or_default();
    Ok((files, value.providers.keys().cloned().collect()))
}

/// The aliases from `label` to the target a configured target stands for,
/// and that target's label.
async fn alias_chain(ctx: &Ctx, label: Label, config: ConfigId, target: &ConfiguredTarget) -> Result<(Vec<Label>, Label), SkyError> {
    let mut aliases = Vec::new();
    let mut current = label;
    let mut configured = (target.label, target.config);
    for _ in 0..64 {
        if !is_alias(ctx, current).await? {
            return Ok((aliases, current));
        }
        aliases.push(current);
        let alias = ctx.compute(&ConfiguredTargetKey { label: configured.0, config: configured.1 }).await?;
        let Some(actual) = alias.deps.first() else {
            return Ok((aliases, current));
        };
        current = actual.label;
        configured = (actual.label, actual.config.unwrap_or(config));
    }
    Ok((aliases, current))
}

/// Whether a label names an `alias` rule.
async fn is_alias(ctx: &Ctx, label: Label) -> Result<bool, SkyError> {
    let package = ctx.compute(&PackageKey(label.package)).await?;
    Ok(match package.0.target(label.name) {
        Some(Target::Rule(rule)) => {
            let class = package.0.rule_class(rule);
            class.definition.is_none() && class.kind.as_str() == "alias"
        }
        _ => false,
    })
}

/// Check one dependency through one attribute.
async fn check_dep(ctx: &Ctx, consumer: &Consumer<'_>, schema: &AttrSchema, found: &Found, checked: &mut Checked) -> Result<(), SkyError> {
    if let Some(requirements) = &schema.requirements {
        if let Some(error) = check_type(ctx, requirements, found).await? {
            checked.errors.push(consumer.attribute_error(schema, &error));
        }
        if let Some(error) = check_file_types(consumer, schema, requirements, found) {
            checked.errors.push(consumer.attribute_error(schema, &error));
        }
    }
    if found.what == What::PackageGroup && schema.requirements.is_some() && !accepts_package_groups(schema) {
        let message = format!("{} is misplaced here (they are only allowed in the visibility attribute)", found.describe(true));
        let error = consumer.attribute_error(schema, &consumer.attribute_error(schema, &message));
        checked.errors.push(error);
    }
    if !matches!(schema.default, AttrDefault::LateBound(_)) {
        if let Some(error) = check_visibility(ctx, consumer, schema, found).await? {
            checked.errors.push(error);
        }
    }
    if let Some(error) = check_testonly(ctx, consumer, found).await? {
        checked.errors.push(error);
    }
    if let Some(warning) = check_deprecation(ctx, consumer, found).await? {
        checked.warnings.push(warning);
    }
    Ok(())
}

/// Whether an attribute may name package groups: `visibility`, the
/// allowlist of transitions, or one that requires their provider.
fn accepts_package_groups(schema: &AttrSchema) -> bool {
    if matches!(schema.name.as_str(), "visibility" | "$allowlist_function_transition" | "_allowlist_function_transition") {
        return true;
    }
    let providers = schema.requirements.iter().flat_map(|requirements| requirements.providers.iter().flatten());
    providers.into_iter().any(|provider| provider.name.contains("PackageSpecification"))
}

/// Whether the dependency is a kind the attribute allows, as Bazel's
/// `validateDirectPrerequisiteType` decides: a rule of a kind it allows or
/// with the providers it requires, or a file of a type it allows.
async fn check_type(ctx: &Ctx, requirements: &DepRequirements, found: &Found) -> Result<Option<String>, SkyError> {
    if found.what == What::Rule {
        return Ok(check_rule(requirements, found));
    }
    if !found.is_file() || requirements.files.allows(found.label.name.as_str()) {
        return Ok(None);
    }
    if found.what == What::SourceFile && !source_exists(ctx, found.label).await? {
        let name = found.label.name.as_str();
        return Ok(Some(match requirements.files == FileTypes::None && !name.contains('.') {
            true => format!("rule '{}' does not exist", found.label),
            false => format!("target '{}' does not exist", found.label),
        }));
    }
    Ok(Some(format!("{} is misplaced here (expected {})", found.describe(true), describe_files(&requirements.files))))
}

/// Whether a rule dependency is of a kind the attribute allows or has the
/// providers it requires; either will do.
fn check_rule(requirements: &DepRequirements, found: &Found) -> Option<String> {
    let mut unfulfilled = Vec::new();
    if !requirements.rules.is_empty() {
        let kind = found.kind.strip_suffix(" rule").unwrap_or(&found.kind);
        if requirements.rules.iter().any(|rule| rule == kind) {
            return None;
        }
        unfulfilled.push(format!("{} is misplaced here (expected {})", found.describe(true), english_list(&requirements.rules, "or")));
    }
    if !requirements.providers.is_empty() {
        let satisfied = requirements.providers.iter().any(|set| set.iter().all(|provider| found.providers.contains(&provider.key)));
        if satisfied {
            return None;
        }
        unfulfilled.push(format!("'{}' does not have mandatory providers: {}", found.label, missing_providers(requirements, found)));
    }
    match unfulfilled.is_empty() {
        true => None,
        false => Some(english_list(&unfulfilled, "and")),
    }
}

/// The providers a dependency lacks of each set an attribute requires, as
/// Bazel describes them: `'A' or ['B', 'C']`.
fn missing_providers(requirements: &DepRequirements, found: &Found) -> String {
    let mut sets = Vec::new();
    for set in &requirements.providers {
        let missing: Vec<String> = set.iter().filter(|provider| !found.providers.contains(&provider.key)).map(|provider| format!("'{}'", provider.name)).collect();
        sets.push(match missing.len() {
            1 => missing.join(""),
            _ => format!("[{}]", missing.join(", ")),
        });
    }
    sets.join(" or ")
}

/// Whether the dependency's files are of a type the attribute allows, as
/// Bazel's `validateDirectPrerequisiteFileTypes` decides: one, if it
/// allows a single file, and at least one of a type it allows.
fn check_file_types(consumer: &Consumer<'_>, schema: &AttrSchema, requirements: &DepRequirements, found: &Found) -> Option<String> {
    if requirements.skip_file_type_check || requirements.files == FileTypes::None {
        return None;
    }
    if requirements.files == FileTypes::Any && !schema.non_empty && !requirements.single_file {
        return None;
    }
    if requirements.single_file && found.files.len() != 1 {
        return Some(format!("'{}' must produce a single file", found.label));
    }
    let allowed = found.files.iter().any(|file| file.is_directory || requirements.files.allows(file.exec_path.name().as_str()));
    if allowed {
        return None;
    }
    Some(format!(
        "'{}' does not produce any {} {} files (expected {})",
        found.label,
        consumer.kind,
        display_name(schema),
        describe_files(&requirements.files)
    ))
}

/// The files an attribute allows, as Bazel's errors describe them.
fn describe_files(files: &FileTypes) -> String {
    match files {
        FileTypes::Any => "any files".to_string(),
        FileTypes::None => "no files".to_string(),
        FileTypes::Extensions(extensions) => english_list(extensions, "or"),
    }
}

/// Items joined as Bazel's `StringUtil.joinEnglishList` joins them: `a`,
/// `a or b`, `a, b, or c`.
fn english_list(items: &[String], last: &str) -> String {
    match items {
        [] => "nothing".to_string(),
        [only] => only.clone(),
        [first, second] => format!("{first} {last} {second}"),
        [rest @ .., final_item] => format!("{}, {last} {final_item}", rest.join(", ")),
    }
}

/// Whether a source file is in its package's directory.
async fn source_exists(ctx: &Ctx, label: Label) -> Result<bool, SkyError> {
    let root = ctx.compute(&RepoRootKey(label.repo())).await?;
    let path = label.package.path.join_path(crate::artifact::name_path(label));
    let state = ctx.compute(&FileStateKey(RootedPath::new(root.0, path))).await?;
    Ok(!matches!(*state, FileState::Missing))
}

/// Whether the rule may see the dependency, as Bazel's visibility check
/// decides, for implicit dependencies of Starlark rules from the `.bzl`
/// file that defines the rule, or else from the rule.
async fn check_visibility(ctx: &Ctx, consumer: &Consumer<'_>, schema: &AttrSchema, found: &Found) -> Result<Option<String>, SkyError> {
    if !consumer.check_visibility {
        return Ok(None);
    }
    let visibility = Visibility::of(ctx, found.visible_as).await?;
    let location = consumer_location(consumer.rule, consumer.label.package, found.visible_as);
    let definition = consumer.definition.filter(|_| schema.name.as_str().starts_with('_'));
    if let Some(definition) = definition {
        if visibility.allows(found.visible_as, definition.package) || visibility.allows(found.visible_as, location) {
            return Ok(None);
        }
        return Ok(Some(visibility_error(consumer, found, definition)));
    }
    if visibility.allows(found.visible_as, location) {
        return Ok(None);
    }
    Ok(Some(visibility_error(consumer, found, consumer.label)))
}

/// The location a rule's dependency's visibility is checked from: where
/// the innermost symbolic macro declaring the rule is defined, unless its
/// caller gave it the dependency, in which case the caller's location,
/// out to the BUILD file's package.
fn consumer_location(rule: &Rule, package: PackageId, dependency: Label) -> PackageId {
    for frame in rule.macros.iter().rev() {
        if !frame.labels.contains(&dependency) {
            return frame.definition;
        }
    }
    package
}

/// Bazel's visibility error, naming where the dependency had to be visible
/// from: the rule, or the `.bzl` file defining it.
fn visibility_error(consumer: &Consumer<'_>, found: &Found, from: Label) -> String {
    let mut message = format!(
        "Visibility error:\n{} is not visible from\ntarget '{from}'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility",
        found.describe(false)
    );
    if found.what == What::SourceFile && found.aliases.is_empty() {
        message.push_str(". To set the visibility of that source file target, use the exports_files() function");
    }
    consumer.rule_error(&message)
}

/// Who may see a target: the packages its visibility names, and, unless a
/// symbolic macro declared it, its own package.
struct Visibility {
    specifications: Vec<PackageSpecification>,
    /// Whether the target's own package always sees it, as for targets the
    /// BUILD file itself declares.
    own_package: bool,
}

impl Visibility {
    async fn of(ctx: &Ctx, label: Label) -> Result<Visibility, SkyError> {
        let package = ctx.compute(&PackageKey(label.package)).await?;
        let package = &package.0;
        let default = || package.args.default_visibility.clone();
        let (labels, macros) = match package.target(label.name) {
            Some(Target::Rule(rule)) => (rule_visibility(rule).unwrap_or_else(default), rule.macros.clone()),
            Some(Target::OutputFile { rule, .. }) => match package.target(*rule) {
                Some(Target::Rule(rule)) => (rule_visibility(rule).unwrap_or_else(default), rule.macros.clone()),
                _ => (default(), Vec::new()),
            },
            Some(Target::SourceFile(file)) => (file.visibility.clone().unwrap_or_else(default), Vec::new()),
            Some(Target::PackageGroup(_)) | None => {
                return Ok(Visibility {
                    specifications: vec![PackageSpecification::everything()],
                    own_package: true,
                });
            }
        };
        let mut specifications = Vec::with_capacity(labels.len() + 1);
        for label in labels {
            specifications.extend(specification(ctx, label).await?);
        }
        if let Some(frame) = macros.last() {
            specifications.push(PackageSpecification::package(frame.definition));
        }
        Ok(Visibility {
            specifications,
            own_package: macros.is_empty(),
        })
    }

    /// Whether `location` may see the target `label`.
    fn allows(&self, label: Label, location: PackageId) -> bool {
        (self.own_package && label.package == location) || self.specifications.iter().any(|specification| specification.contains(location))
    }
}

fn rule_visibility(rule: &Rule) -> Option<Vec<Label>> {
    match rule.attr(Symbol::intern("visibility")) {
        Some(AttrValue::LabelList(labels)) => Some(labels.clone()),
        _ => None,
    }
}

/// The packages one label of a visibility names: by itself, or as a
/// package group's.
async fn specification(ctx: &Ctx, label: Label) -> Result<Option<PackageSpecification>, SkyError> {
    if let Some(specification) = PackageSpecification::of_visibility_label(label) {
        return Ok(Some(specification));
    }
    if matches!(label.name.as_str(), "private") && label.package.path.to_string() == "visibility" {
        return Ok(None);
    }
    let key = ConfiguredTargetKey {
        label,
        config: bazel_config::no_config(),
    };
    let group = ctx.compute(&key).await?;
    let providers = group.providers()?;
    let specification = providers
        .get()
        .and_then(|value| value.provider(crate::package_spec::PROVIDER))
        .and_then(|value| value.downcast_ref::<PackageSpecification>().cloned());
    match specification {
        Some(specification) => Ok(Some(specification)),
        None => Err(SkyError::new(format!("Label '{label}' does not refer to a package group"))),
    }
}

/// Whether a rule that isn't testonly depends on a testonly target, which
/// Bazel forbids. Output files of testonly rules aren't checked, as Bazel
/// doesn't by default.
async fn check_testonly(ctx: &Ctx, consumer: &Consumer<'_>, found: &Found) -> Result<Option<String>, SkyError> {
    if consumer.testonly || can_have_any_provider(consumer.class) || found.what != What::Rule {
        return Ok(None);
    }
    if !target_testonly(ctx, found.label).await? {
        return Ok(None);
    }
    let message = format!("non-test target '{}' depends on testonly {} and doesn't have testonly attribute set", consumer.label, found.describe(false));
    Ok(Some(consumer.rule_error(&message)))
}

/// Whether a rule class may provide anything, as `alias` and the label
/// settings may, so that it may depend on testonly targets.
fn can_have_any_provider(class: &RuleClass) -> bool {
    matches!(class.kind.as_str(), "alias" | "label_flag" | "label_setting")
}

async fn target_testonly(ctx: &Ctx, label: Label) -> Result<bool, SkyError> {
    let package = ctx.compute(&PackageKey(label.package)).await?;
    let Some(Target::Rule(rule)) = package.0.target(label.name) else {
        return Ok(false);
    };
    Ok(match rule.attr(Symbol::intern("testonly")) {
        Some(AttrValue::Bool(testonly)) => *testonly,
        _ => package.0.args.default_testonly,
    })
}

/// The warning for a dependency on a deprecated target in another package,
/// unless the rule is deprecated itself.
async fn check_deprecation(ctx: &Ctx, consumer: &Consumer<'_>, found: &Found) -> Result<Option<String>, SkyError> {
    if consumer.deprecated || found.label.package == consumer.label.package {
        return Ok(None);
    }
    let package = ctx.compute(&PackageKey(found.label.package)).await?;
    let package = &package.0;
    let deprecation = |rule: &Rule| match rule.attr(Symbol::intern("deprecation")) {
        Some(AttrValue::String(message)) => Some(message.clone()),
        _ => package.args.default_deprecation.clone(),
    };
    let message = match package.target(found.label.name) {
        Some(Target::Rule(rule)) => deprecation(rule).map(|warning| format!("target '{}' depends on deprecated target '{}': {warning}", consumer.label, found.label)),
        Some(Target::OutputFile { rule, .. }) => match package.target(*rule) {
            Some(Target::Rule(generating)) => deprecation(generating).map(|warning| {
                let generating = found.label.sibling(*rule);
                format!("target '{}' depends on the output file {} of a deprecated rule {generating}': {warning}", consumer.label, found.label)
            }),
            _ => None,
        },
        _ => None,
    };
    Ok(message.map(|message| consumer.rule_error(&message)))
}

/// The errors for attributes that mustn't be empty and are, as Bazel
/// checks after the dependencies.
fn check_non_empty(consumer: &Consumer<'_>, class: &RuleClass, attrs: &[(Symbol, AttrValue)], checked: &mut Checked) {
    for (schema, (_, value)) in class.attrs.iter().zip(attrs) {
        if schema.non_empty && is_empty(value) {
            checked.errors.push(consumer.attribute_error(schema, "attribute must be non empty"));
        }
    }
}

fn is_empty(value: &AttrValue) -> bool {
    match value {
        AttrValue::LabelList(values) => values.is_empty(),
        AttrValue::StringList(values) => values.is_empty(),
        AttrValue::IntList(values) => values.is_empty(),
        AttrValue::StringDict(values) => values.is_empty(),
        AttrValue::StringListDict(values) => values.is_empty(),
        AttrValue::LabelKeyedStringDict(values) => values.is_empty(),
        AttrValue::StringKeyedLabelDict(values) => values.is_empty(),
        AttrValue::None => true,
        _ => false,
    }
}

/// Whether the rule may see the `config_setting`s its `select()`s name,
/// as Bazel checks by default: those without a visibility of their own
/// are public.
async fn check_conditions(ctx: &Ctx, consumer: &Consumer<'_>, rule: &Rule, checked: &mut Checked) -> Result<(), SkyError> {
    if !consumer.check_visibility {
        return Ok(());
    }
    let mut conditions = Vec::new();
    for (_, value) in &rule.attrs {
        value.conditions(&mut conditions);
    }
    conditions.retain(|condition| condition.to_string() != bazel_packages::DEFAULT_CONDITION);
    conditions.sort();
    conditions.dedup();
    for condition in conditions {
        let actual = crate::platform::actual(ctx, condition).await?;
        let package = ctx.compute(&PackageKey(actual.package)).await?;
        let explicit = match package.0.target(actual.name) {
            Some(Target::Rule(rule)) => package.0.rule_class(rule).kind.as_str() != "config_setting" || rule_visibility(rule).is_some(),
            _ => true,
        };
        if !explicit {
            continue;
        }
        let visibility = Visibility::of(ctx, actual).await?;
        let location = consumer_location(consumer.rule, consumer.label.package, actual);
        if !visibility.allows(actual, location) {
            let found = Found {
                aliases: Vec::new(),
                label: actual,
                what: What::Rule,
                kind: String::new(),
                files: Vec::new(),
                providers: Vec::new(),
                visible_as: actual,
            };
            checked.errors.push(visibility_error(consumer, &found, consumer.label));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_packages::{Location, MacroFrame};

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    #[test]
    fn lists_join_as_bazel_joins_them() {
        let items = |items: &[&str]| items.iter().map(|item| item.to_string()).collect::<Vec<_>>();
        assert_eq!(english_list(&items(&[".txt"]), "or"), ".txt");
        assert_eq!(english_list(&items(&[".a", ".b"]), "or"), ".a or .b");
        assert_eq!(english_list(&items(&[".a", ".b", ".c"]), "or"), ".a, .b, or .c");
    }

    #[test]
    fn macros_check_visibility_where_they_are_defined_unless_given_the_dependency() {
        let package = label("//pkg:BUILD").package;
        let outer = label("//outer:defs.bzl").package;
        let inner = label("//inner:defs.bzl").package;
        let dependency = label("//dep:lib");
        let mut rule = Rule {
            name: Symbol::intern("t"),
            class: 0,
            attrs: Vec::new(),
            location: Location::default(),
            generator: None,
            outputs: Vec::new(),
            computed: Vec::new(),
            call_stack: Vec::new(),
            macros: Vec::new(),
        };
        assert_eq!(consumer_location(&rule, package, dependency), package);
        rule.macros = vec![
            MacroFrame { definition: outer, labels: vec![dependency] },
            MacroFrame { definition: inner, labels: Vec::new() },
        ];
        assert_eq!(consumer_location(&rule, package, dependency), inner);
        rule.macros[1].labels = vec![dependency];
        assert_eq!(consumer_location(&rule, package, dependency), package);
        rule.macros[0].labels.clear();
        assert_eq!(consumer_location(&rule, package, dependency), outer);
    }
}
