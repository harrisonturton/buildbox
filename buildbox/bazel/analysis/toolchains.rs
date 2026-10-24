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
use bazel_packages::{AttrValue, RuleClass};
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
}

/// Resolve and analyze the toolchains a rule class needs.
pub async fn resolve(ctx: &Ctx, class: &RuleClass, config: &BuildConfiguration) -> Result<ResolvedToolchains, SkyError> {
    if class.toolchain_types.is_empty() {
        return Ok(ResolvedToolchains::default());
    }
    let mut types = Vec::with_capacity(class.toolchain_types.len());
    for toolchain_type in &class.toolchain_types {
        types.push(crate::platform::actual(ctx, *toolchain_type).await?);
    }
    let mut optional = Vec::with_capacity(class.optional_toolchain_types.len());
    for toolchain_type in &class.optional_toolchain_types {
        optional.push(crate::platform::actual(ctx, *toolchain_type).await?);
    }
    let key = ToolchainResolutionKey {
        config: config.id,
        types,
        optional,
    };
    let resolution = ctx.compute(&key).await?;
    let keys: Vec<ConfiguredTargetKey> = resolution
        .toolchains
        .iter()
        .filter_map(|(_, toolchain)| *toolchain)
        .map(|label| ConfiguredTargetKey { label, config: config.id })
        .collect();
    let mut analyzed = ctx.compute_many(&keys).await.into_iter();
    let mut resolved = Vec::with_capacity(resolution.toolchains.len());
    for ((_, toolchain), requested) in resolution.toolchains.iter().zip(&class.toolchain_types) {
        let target = match toolchain {
            Some(_) => Some(analyzed.next().ok_or_else(|| SkyError::new("toolchain analysis"))??),
            None => None,
        };
        // Rules look toolchains up by the type they asked for, which may be
        // an alias.
        resolved.push((*requested, target));
    }
    Ok(ResolvedToolchains { resolved })
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegisteredToolchainsKey;

impl Key for RegisteredToolchainsKey {
    type Value = RegisteredToolchains;
    const KIND: &'static str = "REGISTERED_TOOLCHAINS";

    async fn compute(&self, ctx: &Ctx) -> Result<RegisteredToolchains, SkyError> {
        let registrations = ctx.compute(&RegistrationsKey).await?;
        let mut toolchain_patterns = Vec::new();
        let mut platform_patterns = Vec::new();
        for registration in &registrations.0 {
            toolchain_patterns.extend(registration.toolchains.iter().map(|pattern| (registration.repo, pattern.as_str())));
            platform_patterns.extend(registration.execution_platforms.iter().map(|pattern| (registration.repo, pattern.as_str())));
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

/// The rules of `kind` that registration patterns name, in order. Every
/// package the patterns cover is loaded at once.
async fn expand(ctx: &Ctx, patterns: &[(RepoName, &str)], kind: &str) -> Result<Vec<Label>, SkyError> {
    let mut parsing = Vec::with_capacity(patterns.len());
    for (repo, pattern) in patterns {
        parsing.push(pattern_packages(ctx, *repo, pattern));
    }
    let parsed = futures::future::try_join_all(parsing).await?;
    let keys: Vec<PackageKey> = parsed.iter().flat_map(|named| named.packages.iter().map(|package| PackageKey(*package))).collect();
    let mut loaded = ctx.compute_many(&keys).await.into_iter();
    let mut labels = Vec::new();
    for named in parsed {
        labels.extend(named.target);
        for _ in &named.packages {
            let package = loaded.next().ok_or_else(|| SkyError::new("a registered package wasn't loaded"))??;
            for rule in package.0.rules() {
                if package.0.rule_class(rule).kind.as_str() == kind {
                    labels.push(package.0.label(rule.name));
                }
            }
        }
    }
    Ok(labels)
}

/// What a registration pattern names: one target, or the packages whose
/// rules it covers.
struct Named {
    target: Option<Label>,
    packages: Vec<PackageId>,
}

async fn pattern_packages(ctx: &Ctx, repo: RepoName, pattern: &str) -> Result<Named, SkyError> {
    let mapping = ctx.compute(&RepoMappingKey(repo)).await?;
    let context = LabelContext::new(PackageId::new(repo, PathId::ROOT), &mapping.0);
    let parsed = TargetPattern::parse(pattern, &context).map_err(|error| SkyError::new(format!("registering '{pattern}': {error}")))?;
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

/// Chooses toolchains: the first execution platform for which every
/// mandatory type has a toolchain, and for each type the first registered
/// toolchain compatible with it and the target platform.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ToolchainResolutionKey {
    pub config: ConfigId,
    pub types: Vec<Label>,
    pub optional: Vec<Label>,
}

impl Key for ToolchainResolutionKey {
    type Value = ToolchainResolution;
    const KIND: &'static str = "TOOLCHAIN_RESOLUTION";

    async fn compute(&self, ctx: &Ctx) -> Result<ToolchainResolution, SkyError> {
        let config = self.config.get();
        let registered = ctx.compute(&RegisteredToolchainsKey).await?;
        let target = ctx.compute(&PlatformKey(config.options.platform)).await?;
        let mut platforms = registered.execution_platforms.clone();
        platforms.push(config.options.host_platform);
        for platform in platforms {
            let exec = ctx.compute(&PlatformKey(platform)).await?;
            if let Some(toolchains) = self.resolve_for(ctx, &registered, &target, &exec).await? {
                return Ok(ToolchainResolution {
                    exec_platform: platform,
                    toolchains,
                });
            }
        }
        let missing: Vec<String> = self.types.iter().filter(|label| !self.optional.contains(label)).map(Label::to_string).collect();
        Err(SkyError::new(format!(
            "No matching toolchains found for types {}.\nTo debug, rerun with --toolchain_resolution_debug='{}'",
            missing.join(", "),
            missing.first().cloned().unwrap_or_default()
        )))
    }
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
