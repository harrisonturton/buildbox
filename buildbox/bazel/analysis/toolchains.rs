//! Toolchain resolution: choosing, for each toolchain type a rule needs, a
//! toolchain that targets the target platform.

use crate::attrs::resolve_value;
use crate::condition::ConditionKey;
use crate::configured::{ConfiguredTarget, ConfiguredTargetKey};
use crate::platform::{ConstraintValueKey, Platform, PlatformKey, labels};
use crate::target_value::TargetValue;
use allocative::Allocative;
use bazel_config::{BuildConfiguration, ConfigId};
use bazel_label::{Label, LabelContext, PackageId, RepoMappingTable, RepoName, TargetPattern};
use bazel_loading::{PackageKey, RegistrationsKey, RepoMappingKey};
use bazel_packages::{AttrValue, RuleClass, Target};
use bazel_starlark::label_value::LabelValue;
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use starlark::coerce::Coerce;
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;
use std::sync::Arc;

/// The toolchains resolved for a rule: each toolchain type, with the
/// analyzed toolchain, or `None` if none was found for an optional type.
#[derive(Default)]
pub struct ResolvedToolchains {
    pub resolved: Vec<(Label, Option<Arc<ConfiguredTarget>>)>,
    /// The execution platform chosen for the rule's actions and tools.
    pub exec_platform: Option<Label>,
}

/// Choose the execution platform for a rule, whose `exec_compatible_with`
/// is `exec_constraints`, and resolve and analyze the toolchains its class
/// needs for it.
pub async fn resolve(ctx: &Ctx, label: Label, class: &RuleClass, config: &BuildConfiguration, exec_constraints: &[Label]) -> Result<ResolvedToolchains, SkyError> {
    let mut types = Vec::with_capacity(class.toolchain_types.len());
    for toolchain_type in &class.toolchain_types {
        types.push(crate::platform::actual(ctx, *toolchain_type).await?);
    }
    let mut optional = Vec::with_capacity(class.optional_toolchain_types.len());
    for toolchain_type in &class.optional_toolchain_types {
        optional.push(crate::platform::actual(ctx, *toolchain_type).await?);
    }
    let mut constraints = Vec::with_capacity(exec_constraints.len());
    for constraint in exec_constraints {
        constraints.push(crate::platform::actual(ctx, *constraint).await?);
    }
    let key = ToolchainResolutionKey {
        config: config.id,
        types,
        optional,
        exec_constraints: constraints,
        forced_exec_platform: config.toolchain_exec_platform,
    };
    let resolution = ctx.compute(&key).await?;
    let toolchain_config = config.for_toolchain(resolution.exec_platform);
    let keys: Vec<ConfiguredTargetKey> = resolution
        .toolchains
        .iter()
        .filter_map(|(_, toolchain)| *toolchain)
        .map(|label| ConfiguredTargetKey { label, config: toolchain_config })
        .collect();
    let mut analyzed = ctx.compute_many(&keys).await.into_iter();
    let mut resolved = Vec::with_capacity(resolution.toolchains.len());
    for ((_, toolchain), requested) in resolution.toolchains.iter().zip(&class.toolchain_types) {
        let target = match toolchain {
            Some(toolchain) => match analyzed.next().ok_or_else(|| SkyError::new("toolchain analysis"))? {
                Ok(target) => Some(target),
                Err(error) if error.origin().is_some() => return Err(error),
                Err(error) => return Err(unfound_toolchain(ctx, label, *toolchain, error).await),
            },
            None => None,
        };
        // Rules look toolchains up by the type they asked for, which may be
        // an alias.
        resolved.push((*requested, target));
    }
    Ok(ResolvedToolchains {
        resolved,
        exec_platform: Some(resolution.exec_platform),
    })
}

/// Bazel's error for a toolchain target that can't be had: why, at its
/// package's BUILD file, then that the target needing it depends on it.
async fn unfound_toolchain(ctx: &Ctx, label: Label, toolchain: Label, error: SkyError) -> SkyError {
    let build_file = bazel_loading::build_file_path(ctx, toolchain.package).await.ok().flatten();
    let cause = match build_file {
        Some(path) => format!("{}: {}", path.display(), error.message()),
        None => error.message().to_string(),
    };
    let depends = format!("Target '{label}' depends on toolchain '{toolchain}', which cannot be found: {}'", error.message());
    let depends = match bazel_loading::target_location(ctx, label).await {
        Some(location) => format!("{location}: {depends}"),
        None => depends,
    };
    SkyError::with_kind(error.kind(), crate::configured::errors(&[&cause, &depends])).with_origin(toolchain.to_string())
}

/// A `toolchain` target: what it's for, and where it applies.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct DeclaredToolchain {
    pub label: Label,
    pub toolchain_type: Label,
    /// The target implementing the toolchain, which may be a `select()`.
    pub toolchain: AttrValue,
    pub target_compatible_with: Vec<Label>,
    pub exec_compatible_with: Vec<Label>,
    pub target_settings: Vec<Label>,
}

/// The toolchains and execution platforms the build registers, in order of
/// precedence.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RegisteredToolchains {
    pub toolchains: Vec<DeclaredToolchain>,
    pub execution_platforms: Vec<Label>,
}

encoded_value!(RegisteredToolchains);

/// The toolchains and execution platforms a configuration registers: those
/// its `--extra_toolchains` and `--extra_execution_platforms` name, then
/// those modules register. As in Bazel, the last `--extra_toolchains` comes
/// first.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegisteredToolchainsKey {
    pub extra_toolchains: Vec<String>,
    pub extra_execution_platforms: Vec<String>,
}

impl RegisteredToolchainsKey {
    pub fn of(config: &BuildConfiguration) -> RegisteredToolchainsKey {
        RegisteredToolchainsKey {
            extra_toolchains: config.options.strings("extra_toolchains").to_vec(),
            extra_execution_platforms: config.options.strings("extra_execution_platforms").to_vec(),
        }
    }
}

impl Key for RegisteredToolchainsKey {
    type Value = RegisteredToolchains;
    const KIND: &'static str = "REGISTERED_TOOLCHAINS";

    async fn compute(&self, ctx: &Ctx) -> Result<RegisteredToolchains, SkyError> {
        let registrations = ctx.compute(&RegistrationsKey).await?;
        let mut toolchain_patterns: Vec<Registered<'_>> = self.extra_toolchains.iter().rev().map(|pattern| Registered::flag(pattern)).collect();
        let mut platform_patterns: Vec<Registered<'_>> = self.extra_execution_platforms.iter().map(|pattern| Registered::flag(pattern)).collect();
        for registration in &registrations.0 {
            toolchain_patterns.extend(registration.toolchains.iter().map(|pattern| Registered::module(registration.repo, pattern)));
            platform_patterns.extend(registration.execution_platforms.iter().map(|pattern| Registered::module(registration.repo, pattern)));
        }
        let (labels, execution_platforms) = futures::try_join!(
            expand(ctx, &toolchain_patterns, "toolchain"),
            expand(ctx, &platform_patterns, "platform")
        )?;
        let mut declared = Vec::with_capacity(labels.len());
        for label in labels {
            declared.push(declared_toolchain(ctx, label));
        }
        let toolchains = futures::future::try_join_all(declared).await?;
        Ok(RegisteredToolchains {
            toolchains,
            execution_platforms,
        })
    }
}

/// A pattern that registers toolchains or platforms: in the repository it's
/// relative to, and whether it takes away from those before it, as
/// `-//pkg:all` does on the command line.
struct Registered<'a> {
    repo: RepoName,
    pattern: &'a str,
    negative: bool,
}

impl<'a> Registered<'a> {
    fn flag(pattern: &'a str) -> Registered<'a> {
        match pattern.strip_prefix('-') {
            Some(pattern) => Registered { repo: RepoName::MAIN, pattern, negative: true },
            None => Registered { repo: RepoName::MAIN, pattern, negative: false },
        }
    }

    fn module(repo: RepoName, pattern: &'a str) -> Registered<'a> {
        Registered { repo, pattern, negative: false }
    }
}

/// The rules of `kind` that registration patterns name, in order, each
/// once. Every package the patterns cover is loaded at once.
async fn expand(ctx: &Ctx, patterns: &[Registered<'_>], kind: &str) -> Result<Vec<Label>, SkyError> {
    let mut parsing = Vec::with_capacity(patterns.len());
    for registered in patterns {
        parsing.push(pattern_packages(ctx, registered, kind));
    }
    let parsed = futures::future::try_join_all(parsing).await?;
    let keys: Vec<PackageKey> = parsed.iter().flat_map(|named| named.packages.iter().map(|package| PackageKey(*package))).collect();
    let mut loaded = ctx.compute_many(&keys).await.into_iter();
    let mut labels: Vec<Label> = Vec::new();
    for (registered, named) in patterns.iter().zip(parsed) {
        let mut matched = Vec::new();
        if let Some(label) = named.target {
            matched.push(explicit(ctx, registered, label, kind).await?);
        }
        for _ in &named.packages {
            let package = loaded.next().ok_or_else(|| SkyError::new("a registered package wasn't loaded"))?;
            let package = package.map_err(|error| invalid_registration(registered, kind, error.message()))?;
            for rule in package.0.rules() {
                if package.0.rule_class(rule).kind.as_str() == kind {
                    matched.push(package.0.label(rule.name));
                }
            }
        }
        match registered.negative {
            true => labels.retain(|label| !matched.contains(label)),
            false => labels.extend(matched.into_iter().filter(|label| !labels.contains(label)).collect::<Vec<_>>()),
        }
    }
    Ok(labels)
}

/// A target a registration names explicitly, which must exist and, through
/// aliases, be of `kind`.
async fn explicit(ctx: &Ctx, registered: &Registered<'_>, label: Label, kind: &str) -> Result<Label, SkyError> {
    let package = ctx.compute(&PackageKey(label.package)).await.map_err(|error| invalid_registration(registered, kind, error.message()))?;
    if package.0.target(label.name).is_none() {
        let build_file = bazel_loading::build_file_path(ctx, label.package).await?.unwrap_or_default();
        return Err(invalid_registration(registered, kind, &bazel_loading::no_such_target(label, &build_file)));
    }
    let actual = crate::platform::actual(ctx, label).await?;
    let package = ctx.compute(&PackageKey(actual.package)).await?;
    let is_kind = match package.0.target(actual.name) {
        Some(Target::Rule(rule)) => package.0.rule_class(rule).kind.as_str() == kind,
        _ => false,
    };
    match (is_kind, kind) {
        (true, _) => Ok(label),
        (false, "toolchain") => Err(invalid_registration(registered, kind, "target does not provide the DeclaredToolchainInfo provider")),
        (false, _) => Err(SkyError::new(format!("Target {label} was referenced as a platform, but does not provide PlatformInfo"))),
    }
}

/// Bazel's error for a registration of toolchains or execution platforms
/// that can't be had.
fn invalid_registration(registered: &Registered<'_>, kind: &str, reason: &str) -> SkyError {
    let what = match kind {
        "toolchain" => "toolchain",
        _ => "execution platform",
    };
    let sign = if registered.negative { "-" } else { "" };
    SkyError::new(format!("invalid registered {what} '{sign}{}': {reason}", registered.pattern))
}

/// What a registration pattern names: one target, or the packages whose
/// rules it covers.
struct Named {
    target: Option<Label>,
    packages: Vec<PackageId>,
}

async fn pattern_packages(ctx: &Ctx, registered: &Registered<'_>, kind: &str) -> Result<Named, SkyError> {
    let mapping = ctx.compute(&RepoMappingKey(registered.repo)).await?;
    let context = LabelContext::new(PackageId::new(registered.repo, PathId::ROOT), &mapping.0);
    let parsed = TargetPattern::parse(registered.pattern, &context).map_err(|error| invalid_registration(registered, kind, &error.to_string()))?;
    let packages = match parsed {
        TargetPattern::Target(label) => {
            return Ok(Named {
                target: Some(label),
                packages: Vec::new(),
            });
        }
        TargetPattern::Package { package, .. } => vec![package],
        TargetPattern::Recursive { repo, prefix, .. } => bazel_loading::packages_beneath(ctx, repo, prefix).await?,
    };
    Ok(Named { target: None, packages })
}

async fn declared_toolchain(ctx: &Ctx, label: Label) -> Result<DeclaredToolchain, SkyError> {
    let rule = crate::platform::load_rule(ctx, label, "toolchain").await?;
    let Some(AttrValue::Label(toolchain_type)) = rule.attr(Symbol::intern("toolchain_type")).cloned() else {
        return Err(SkyError::new(format!("{label}: toolchain has no toolchain_type")));
    };
    let toolchain_type = crate::platform::actual(ctx, toolchain_type).await?;
    Ok(DeclaredToolchain {
        label,
        toolchain_type,
        toolchain: rule.attr(Symbol::intern("toolchain")).cloned().unwrap_or(AttrValue::None),
        target_compatible_with: labels(&rule, "target_compatible_with"),
        exec_compatible_with: labels(&rule, "exec_compatible_with"),
        target_settings: labels(&rule, "target_settings"),
    })
}

/// The toolchains chosen for a set of toolchain types in a configuration,
/// and the execution platform they're for.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ToolchainResolution {
    pub exec_platform: Label,
    /// Each type, with the target implementing its toolchain.
    pub toolchains: Vec<(Label, Option<Label>)>,
}

encoded_value!(ToolchainResolution);

/// Chooses toolchains: the first execution platform that has the
/// constraints and for which every mandatory type has a toolchain, and for
/// each type the first registered toolchain compatible with it and the
/// target platform.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ToolchainResolutionKey {
    pub config: ConfigId,
    pub types: Vec<Label>,
    pub optional: Vec<Label>,
    pub exec_constraints: Vec<Label>,
    /// For a toolchain target: the execution platform of the rule it's
    /// for, the only one it may use.
    pub forced_exec_platform: Option<Label>,
}

impl Key for ToolchainResolutionKey {
    type Value = ToolchainResolution;
    const KIND: &'static str = "TOOLCHAIN_RESOLUTION";

    async fn compute(&self, ctx: &Ctx) -> Result<ToolchainResolution, SkyError> {
        let config = self.config.get();
        let registered = ctx.compute(&RegisteredToolchainsKey::of(&config)).await?;
        let target = ctx.compute(&PlatformKey(config.platform)).await?;
        let platforms = match self.forced_exec_platform {
            Some(platform) => vec![platform],
            None => execution_platforms(&registered, config.host_platform),
        };
        let mut allowed = Vec::with_capacity(platforms.len());
        for platform in platforms {
            let exec = ctx.compute(&PlatformKey(platform)).await?;
            if satisfies(ctx, &exec, &self.exec_constraints).await? {
                allowed.push((platform, exec));
            }
        }
        for (platform, exec) in &allowed {
            if let Some(toolchains) = self.resolve_for(ctx, &registered, &target, exec).await? {
                return Ok(ToolchainResolution {
                    exec_platform: *platform,
                    toolchains,
                });
            }
        }
        Err(self.unresolved(ctx, &registered, &target, &allowed).await?)
    }
}

/// What the target platform says when toolchains are missing, unless its
/// `missing_toolchain_error` says otherwise.
const DEFAULT_MISSING_TOOLCHAIN_ERROR: &str = "For more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro.";

impl ToolchainResolutionKey {
    /// Why no execution platform has the toolchains, as Bazel says: the
    /// mandatory types no allowed platform has a toolchain for, or else
    /// that no one platform has them all.
    async fn unresolved(&self, ctx: &Ctx, registered: &RegisteredToolchains, target: &Platform, allowed: &[(Label, Arc<Platform>)]) -> Result<SkyError, SkyError> {
        let mut missing = Vec::new();
        for toolchain_type in self.types.iter().filter(|toolchain_type| !self.optional.contains(toolchain_type)) {
            if !self.resolvable(ctx, registered, target, allowed, *toolchain_type).await? {
                missing.push(*toolchain_type);
            }
        }
        if missing.is_empty() {
            return Ok(SkyError::new(no_matching_platform(&self.types, target.label, allowed)));
        }
        let mut rows = Vec::with_capacity(missing.len());
        for toolchain_type in &missing {
            let rule = crate::platform::load_rule(ctx, *toolchain_type, "toolchain_type").await?;
            match rule.attr(Symbol::intern("no_match_error")) {
                Some(AttrValue::String(error)) if !error.is_empty() => rows.push(format!("  {toolchain_type}: {error}")),
                _ => rows.push(format!("  {toolchain_type}")),
            }
        }
        let platform = crate::platform::load_rule(ctx, target.label, "platform").await?;
        let platform_error = match platform.attr(Symbol::intern("missing_toolchain_error")) {
            Some(AttrValue::String(error)) => error.clone(),
            _ => DEFAULT_MISSING_TOOLCHAIN_ERROR.to_string(),
        };
        let debug: Vec<String> = missing.iter().map(Label::to_string).collect();
        Ok(SkyError::new(format!(
            "No matching toolchains found for types:\n{}\nTo debug, rerun with --toolchain_resolution_debug='{}'\n{platform_error}",
            rows.join("\n"),
            debug.join("|")
        )))
    }

    /// Whether any allowed execution platform has a toolchain of a type.
    async fn resolvable(&self, ctx: &Ctx, registered: &RegisteredToolchains, target: &Platform, allowed: &[(Label, Arc<Platform>)], toolchain_type: Label) -> Result<bool, SkyError> {
        for (_, exec) in allowed {
            for declared in registered.toolchains.iter().filter(|declared| declared.toolchain_type == toolchain_type) {
                if self.matches(ctx, declared, target, exec).await? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

/// Bazel's error for toolchains each found on some execution platform, but
/// not all on one, or for no platform with a rule's constraints.
fn no_matching_platform(types: &[Label], target: Label, allowed: &[(Label, Arc<Platform>)]) -> String {
    let available: Vec<String> = allowed.iter().map(|(label, _)| label.to_string()).collect();
    if types.is_empty() {
        return format!("Unable to find an execution platform for target platform {target} from available execution platforms [{}]", available.join(", "));
    }
    let types: Vec<String> = types.iter().map(Label::to_string).collect();
    format!(
        "Unable to find an execution platform for toolchains [{}] and target platform {target} from available execution platforms [{}]",
        types.join(", "),
        available.join(", ")
    )
}

impl ToolchainResolutionKey {
    /// The toolchains for an execution platform, or `None` if a mandatory
    /// type has none.
    async fn resolve_for(
        &self,
        ctx: &Ctx,
        registered: &RegisteredToolchains,
        target: &Platform,
        exec: &Platform,
    ) -> Result<Option<Vec<(Label, Option<Label>)>>, SkyError> {
        let mut chosen = Vec::with_capacity(self.types.len());
        for toolchain_type in &self.types {
            let mut found = None;
            for declared in registered.toolchains.iter().filter(|declared| declared.toolchain_type == *toolchain_type) {
                if self.matches(ctx, declared, target, exec).await? {
                    found = Some(declared);
                    break;
                }
            }
            let toolchain = match found {
                Some(declared) => Some(self.implementation(ctx, declared).await?),
                None if self.optional.contains(toolchain_type) => None,
                None => return Ok(None),
            };
            chosen.push((*toolchain_type, toolchain));
        }
        Ok(Some(chosen))
    }

    async fn matches(&self, ctx: &Ctx, declared: &DeclaredToolchain, target: &Platform, exec: &Platform) -> Result<bool, SkyError> {
        if !satisfies(ctx, target, &declared.target_compatible_with).await? || !satisfies(ctx, exec, &declared.exec_compatible_with).await? {
            return Ok(false);
        }
        for setting in &declared.target_settings {
            let condition = ctx.compute(&ConditionKey { label: *setting, config: self.config }).await?;
            if !condition.matches {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn implementation(&self, ctx: &Ctx, declared: &DeclaredToolchain) -> Result<Label, SkyError> {
        match resolve_value(ctx, &declared.toolchain, self.config).await? {
            AttrValue::Label(label) => Ok(label),
            _ => Err(SkyError::new(format!("{}: toolchain has no 'toolchain'", declared.label))),
        }
    }
}

/// The execution platforms toolchain resolution chooses from, in order:
/// those registered, then the host platform, each once.
fn execution_platforms(registered: &RegisteredToolchains, host_platform: Label) -> Vec<Label> {
    let mut platforms = registered.execution_platforms.clone();
    if !platforms.contains(&host_platform) {
        platforms.push(host_platform);
    }
    platforms
}

/// Whether a platform has every one of some constraint values.
async fn satisfies(ctx: &Ctx, platform: &Platform, constraints: &[Label]) -> Result<bool, SkyError> {
    let keys: Vec<ConstraintValueKey> = constraints.iter().map(|label| ConstraintValueKey(*label)).collect();
    for value in ctx.compute_many(&keys).await {
        let value = value?;
        if !platform.has(&value) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `ctx.toolchains`: the `ToolchainInfo` of each resolved toolchain, by
/// toolchain type.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ToolchainContextGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    pub types: Vec<Label>,
    /// `ToolchainInfo` or `None`, in the order of `types`.
    pub values: Vec<V>,
    /// How the rule's `.bzl` file sees repositories, which string indexes
    /// are labels in.
    #[freeze(identity)]
    #[allocative(skip)]
    pub mapping: Option<(RepoName, Arc<RepoMappingTable>)>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ToolchainContextGen<To>> for ToolchainContextGen<From> {}

starlark_complex_value!(pub ToolchainContext);

impl<'v> ToolchainContext<'v> {
    pub fn new(resolved: &ResolvedToolchains, mapping: Option<(RepoName, Arc<RepoMappingTable>)>, heap: Heap<'v>) -> ToolchainContext<'v> {
        let mut types = Vec::new();
        let mut values = Vec::new();
        for (toolchain_type, toolchain) in &resolved.resolved {
            let value = toolchain
                .as_ref()
                .and_then(|toolchain| {
                    let value = heap.access_owned_frozen_value(toolchain.providers().ok()?.owned());
                    TargetValue::from_value(value)?.provider("ToolchainInfo")
                })
                .unwrap_or_else(Value::new_none);
            types.push(*toolchain_type);
            values.push(value);
        }
        ToolchainContext { types, values, mapping }
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for ToolchainContextGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<toolchain_context>")
    }
}

impl<'v, V: ValueLike<'v>> ToolchainContextGen<V> {
    /// The position of the toolchain type an index names: a label, or a
    /// string, which may name the type's repository by an apparent name.
    fn position(&self, index: Value<'v>) -> Option<usize> {
        if let Some(label) = index.downcast_ref::<LabelValue>() {
            return self.types.iter().position(|toolchain_type| *toolchain_type == label.0);
        }
        let text = index.unpack_str()?;
        if let Some((repo, mapping)) = &self.mapping {
            let context = LabelContext::new(PackageId::new(*repo, PathId::ROOT), mapping.as_ref());
            let position = Label::parse(text, &context).ok().and_then(|label| self.types.iter().position(|toolchain_type| *toolchain_type == label));
            if position.is_some() {
                return position;
            }
        }
        if let Ok(label) = Label::parse_canonical(text) {
            if let Some(position) = self.types.iter().position(|toolchain_type| *toolchain_type == label) {
                return Some(position);
            }
        }
        self.types.iter().position(|toolchain_type| names(*toolchain_type, text))
    }
}

/// Whether text such as `@rules_rust//rust:toolchain_type` names a label,
/// taking its repository name as apparent: the canonical name or the module
/// name it starts with.
fn names(label: Label, text: &str) -> bool {
    let Some((repo, rest)) = text.trim_start_matches('@').split_once("//") else {
        return false;
    };
    let (package, name) = rest.split_once(':').unwrap_or((rest, rest.rsplit('/').next().unwrap_or(rest)));
    if label.package.path.to_string() != package || label.name.as_str() != name {
        return false;
    }
    let canonical = label.repo().as_str();
    repo.is_empty() && label.repo().is_main() || canonical == repo || canonical.split('+').next() == Some(repo)
}

#[starlark_value(type = "ToolchainContext")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ToolchainContextGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        match self.position(index) {
            Some(position) => Ok(self.values[position].to_value()),
            None => Err(starlark::Error::new_other(anyhow::anyhow!(
                "toolchain type {} was requested but only types [{}] are configured",
                index.to_str(),
                self.types.iter().map(|label| label.to_string()).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(self.position(other).is_some())
    }
}
