//! `PackageSpecificationInfo`: the packages a `package_group` contains, as
//! its `packages` give them, with those of the groups it includes.

use allocative::Allocative;
use bazel_label::{Label, PackageId, RepoMapping, RepoName};
use bazel_starlark::label_value::LabelValue;
use intern::PathId;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkValue, Value, ValueLike, starlark_value};
use starlark::{StarlarkPagablePanic, starlark_simple_value};
use std::fmt;

/// The name of the provider, as targets are indexed by it.
pub const PROVIDER: &str = "PackageSpecificationInfo";

/// Which packages one entry of `packages` names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Spec {
    /// `public`: every package.
    Everything,
    /// `//...` or `@repo//...`: every package of a repository.
    Repository(RepoName),
    /// `//pkg`: the package.
    Exactly(PackageId),
    /// `//pkg/...`: the package and those beneath it.
    Beneath(PackageId),
}

impl Spec {
    fn contains(&self, package: PackageId) -> bool {
        match self {
            Spec::Everything => true,
            Spec::Repository(repo) => package.repo == *repo,
            Spec::Exactly(spec) => package == *spec,
            Spec::Beneath(spec) => package.repo == spec.repo && package.path.starts_with(spec.path),
        }
    }
}

/// A package group's packages: those an included spec names and no
/// excluded one does.
#[derive(Clone, Debug, Default, PartialEq, Eq, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct PackageSpecification {
    #[allocative(skip)]
    included: Vec<Spec>,
    #[allocative(skip)]
    excluded: Vec<Spec>,
}

starlark_simple_value!(PackageSpecification);

impl PackageSpecification {
    /// The packages a group in a package of `repo` names, as `mapping` sees
    /// repositories, with those of the groups it includes.
    pub fn new(packages: &[String], repo: RepoName, mapping: &dyn RepoMapping, includes: &[&PackageSpecification]) -> anyhow::Result<PackageSpecification> {
        let mut specification = PackageSpecification::default();
        for text in packages {
            let (negated, text) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text.as_str()),
            };
            let Some(spec) = parse(text, repo, mapping)? else {
                continue;
            };
            match negated {
                true => specification.excluded.push(spec),
                false => specification.included.push(spec),
            }
        }
        for include in includes {
            specification.included.extend(include.included.iter().cloned());
            specification.excluded.extend(include.excluded.iter().cloned());
        }
        Ok(specification)
    }

    pub fn contains(&self, package: PackageId) -> bool {
        self.included.iter().any(|spec| spec.contains(package)) && !self.excluded.iter().any(|spec| spec.contains(package))
    }
}

/// One entry of `packages`, without a leading `-`. `private` names no
/// packages.
fn parse(text: &str, repo: RepoName, mapping: &dyn RepoMapping) -> anyhow::Result<Option<Spec>> {
    match text {
        "public" => return Ok(Some(Spec::Everything)),
        "private" => return Ok(None),
        _ => {}
    }
    let invalid = || anyhow::anyhow!("invalid package specification '{text}'");
    let (repo, path) = match text.split_once("//") {
        Some(("", path)) => (repo, path),
        Some((name, path)) => (repository(name, mapping).ok_or_else(invalid)?, path),
        None => return Err(invalid()),
    };
    if path == "..." {
        return Ok(Some(Spec::Repository(repo)));
    }
    let (path, beneath) = match path.strip_suffix("/...") {
        Some(path) => (path, true),
        None => (path, false),
    };
    let path = PathId::parse(path).map_err(|_| invalid())?;
    let package = PackageId::new(repo, path);
    Ok(Some(match beneath {
        true => Spec::Beneath(package),
        false => Spec::Exactly(package),
    }))
}

/// The repository `@name` or `@@name` names.
fn repository(name: &str, mapping: &dyn RepoMapping) -> Option<RepoName> {
    if let Some(canonical) = name.strip_prefix("@@") {
        return Some(RepoName::new(canonical));
    }
    mapping.resolve(name.strip_prefix('@')?)
}

impl fmt::Display for PackageSpecification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{PROVIDER}>")
    }
}

#[starlark_value(type = "PackageSpecificationInfo")]
impl<'v> StarlarkValue<'v> for PackageSpecification {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("PackageSpecificationInfo", package_specification_methods);
        Some(METHODS.methods())
    }
}

#[starlark_module]
fn package_specification_methods(builder: &mut MethodsBuilder) {
    /// Whether the group contains the package of `target`, a label.
    fn contains<'v>(this: Value<'v>, #[starlark(require = pos)] target: Value<'v>) -> anyhow::Result<bool> {
        let specification = this.downcast_ref::<PackageSpecification>().ok_or_else(|| anyhow::anyhow!("expected a PackageSpecificationInfo"))?;
        let label: Label = match target.downcast_ref::<LabelValue>() {
            Some(label) => label.0,
            None => anyhow::bail!("PackageSpecificationInfo.contains: expected a Label, got {}", target.get_type()),
        };
        Ok(specification.contains(label.package))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(repo: &str, path: &str) -> PackageId {
        PackageId::new(RepoName::new(repo), PathId::parse(path).unwrap())
    }

    fn specification(packages: &[&str]) -> PackageSpecification {
        let mapping = |name: &str| (name == "dep").then(|| RepoName::new("dep+"));
        let packages: Vec<String> = packages.iter().map(|text| text.to_string()).collect();
        PackageSpecification::new(&packages, RepoName::MAIN, &mapping, &[]).unwrap()
    }

    #[test]
    fn packages_are_matched_by_their_specs() {
        let specification = specification(&["//a", "//b/...", "-//b/secret", "@dep//..."]);
        assert!(specification.contains(package("", "a")));
        assert!(!specification.contains(package("", "a/sub")));
        assert!(specification.contains(package("", "b")));
        assert!(specification.contains(package("", "b/c/d")));
        assert!(!specification.contains(package("", "b/secret")));
        assert!(specification.contains(package("dep+", "x")));
        assert!(!specification.contains(package("other+", "x")));
    }

    #[test]
    fn public_and_private_name_everything_and_nothing() {
        assert!(specification(&["public"]).contains(package("other+", "x")));
        assert!(!specification(&["private"]).contains(package("", "")));
    }
}
