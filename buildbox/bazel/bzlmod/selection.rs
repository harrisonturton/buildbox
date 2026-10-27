//! Choosing one version of each module, as Bazel's `Selection` does:
//! minimal version selection within each compatibility level, with
//! `multiple_version_override` keeping several versions of a module, and
//! `max_compatibility_level` letting a dependency accept higher levels.

use crate::version::Version;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;

/// A module version: its name and version, the version empty for a module
/// with a non-registry override. The root module's name is empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleKey {
    pub name: String,
    pub version: String,
}

impl ModuleKey {
    pub fn root() -> ModuleKey {
        ModuleKey {
            name: String::new(),
            version: String::new(),
        }
    }
}

/// As Bazel shows it: `name@version`, `name@_` without a version, or
/// `<root>`.
impl fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.name.is_empty(), self.version.is_empty()) {
            (true, _) => write!(f, "<root>"),
            (false, true) => write!(f, "{}@_", self.name),
            (false, false) => write!(f, "{}@{}", self.name, self.version),
        }
    }
}

/// A dependency as a module declares it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DepSpec {
    pub name: String,
    pub version: String,
    /// The highest compatibility level it accepts, or -1 for only its
    /// version's.
    pub max_compatibility_level: i64,
}

impl DepSpec {
    fn key(&self) -> ModuleKey {
        ModuleKey {
            name: self.name.clone(),
            version: self.version.clone(),
        }
    }
}

/// A module version found while discovering the graph.
#[derive(Clone, Debug)]
pub struct Interim {
    pub compatibility_level: i64,
    /// Its dependencies, by the names it gives their repositories.
    pub deps: Vec<(String, DepSpec)>,
    /// Dependencies that only constrain selection, without a repository.
    pub nodep_deps: Vec<DepSpec>,
}

/// A selected module version, and the versions its dependencies resolve
/// to, by repository name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selected {
    pub key: ModuleKey,
    pub deps: Vec<(String, ModuleKey)>,
}

/// The modules of a selection, in the order Bazel walks them from the
/// root, and what each dependency spec resolved to.
pub struct Selection {
    pub modules: Vec<Selected>,
    resolved: HashMap<DepSpec, String>,
}

impl Selection {
    /// The version `dep` resolved to.
    pub fn resolved(&self, dep: &DepSpec) -> Option<&str> {
        self.resolved.get(dep).map(String::as_str)
    }
}

/// The modules with multiple versions, and the versions each allows.
pub type MultipleVersions = HashMap<String, Vec<String>>;

/// Versions of a module that are selected together.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Group {
    name: String,
    compatibility_level: i64,
    /// The lowest version a `multiple_version_override` allows at or above
    /// the module's, or empty, which sorts last, if there's no override.
    target: String,
}

/// Select the versions of `graph`, which maps each module version found to
/// what it declares; `order` is the order they were found in, the root
/// first.
pub fn select(graph: &HashMap<ModuleKey, Interim>, order: &[ModuleKey], multiple: &MultipleVersions) -> Result<Selection, String> {
    let allowed = allowed_versions(graph, multiple)?;
    let mut groups: HashMap<ModuleKey, Group> = HashMap::new();
    let mut selected: HashMap<Group, Version> = HashMap::new();
    for key in order {
        let group = group_of(key, &graph[key], &allowed)?;
        let version = parse(&key.version)?;
        let best = selected.entry(group.clone()).or_insert_with(|| version.clone());
        if version > *best {
            *best = version;
        }
        groups.insert(key.clone(), group);
    }
    let possibilities = possible_resolutions(graph, order, &groups, &selected)?;
    let walker = Walker { graph, groups: &groups, multiple };
    let mut first_failure = None;
    for strategy in strategies(&possibilities) {
        let walked = walker.walk(&strategy, false).and_then(|_| walker.walk(&strategy, true));
        match walked {
            Ok(modules) => return Ok(Selection { modules, resolved: strategy }),
            Err(error) => {
                first_failure.get_or_insert(error);
            }
        }
    }
    Err(first_failure.unwrap_or_else(|| "no module versions to select".to_string()))
}

fn parse(version: &str) -> Result<Version, String> {
    Version::parse(version).map_err(|error| error.to_string())
}

/// The versions each `multiple_version_override` allows, by module name and
/// compatibility level, each set with the empty version.
fn allowed_versions(graph: &HashMap<ModuleKey, Interim>, multiple: &MultipleVersions) -> Result<HashMap<(String, i64), Vec<Version>>, String> {
    let mut allowed: HashMap<(String, i64), Vec<Version>> = HashMap::new();
    for (name, versions) in multiple {
        for version in versions {
            let key = ModuleKey { name: name.clone(), version: version.clone() };
            let Some(module) = graph.get(&key) else {
                return Err(format!("multiple_version_override for module {name} contains version {version}, but it doesn't exist in the dependency graph"));
            };
            let set = allowed.entry((name.clone(), module.compatibility_level)).or_insert_with(|| vec![Version::empty()]);
            set.push(parse(version)?);
        }
    }
    for set in allowed.values_mut() {
        set.sort();
    }
    Ok(allowed)
}

fn group_of(key: &ModuleKey, module: &Interim, allowed: &HashMap<(String, i64), Vec<Version>>) -> Result<Group, String> {
    let version = parse(&key.version)?;
    let target = match allowed.get(&(key.name.clone(), module.compatibility_level)) {
        Some(set) => set.iter().find(|candidate| **candidate >= version).cloned().unwrap_or_else(Version::empty),
        None => Version::empty(),
    };
    let target = target.as_str().to_string();
    Ok(Group {
        name: key.name.clone(),
        compatibility_level: module.compatibility_level,
        target,
    })
}

/// For each dependency spec, the versions it could resolve to: the
/// selected version of each compatibility level it accepts whose allowed
/// version is at least its own, the lowest of each level, by level.
fn possible_resolutions(
    graph: &HashMap<ModuleKey, Interim>,
    order: &[ModuleKey],
    groups: &HashMap<ModuleKey, Group>,
    selected: &HashMap<Group, Version>,
) -> Result<Vec<(DepSpec, Vec<String>)>, String> {
    let mut results: Vec<(DepSpec, Vec<String>)> = Vec::new();
    for key in order {
        let module = &graph[key];
        let specs = module.deps.iter().map(|(_, spec)| spec).chain(&module.nodep_deps);
        for spec in specs {
            if results.iter().any(|(seen, _)| seen == spec) {
                continue;
            }
            let Some(own) = groups.get(&spec.key()) else {
                return Err(format!("{} wasn't found while discovering the dependency graph", spec.key()));
            };
            let min = own.compatibility_level;
            let max = if spec.max_compatibility_level < 0 { min } else { spec.max_compatibility_level };
            let wanted = parse(&spec.version)?;
            let mut by_level: BTreeMap<i64, Version> = BTreeMap::new();
            for (group, version) in selected {
                let fits = group.name == spec.name && (min..=max).contains(&group.compatibility_level) && parse(&group.target)? >= wanted;
                if !fits {
                    continue;
                }
                let lowest = by_level.entry(group.compatibility_level).or_insert_with(|| version.clone());
                if version < lowest {
                    *lowest = version.clone();
                }
            }
            if by_level.is_empty() {
                return Err(format!("Unexpected error: {} has no valid resolution result", spec.key()));
            }
            results.push((spec.clone(), by_level.into_values().map(|version| version.as_str().to_string()).collect()));
        }
    }
    Ok(results)
}

/// Every way of resolving each dependency spec to one of its possible
/// versions, the first choices first.
fn strategies(possibilities: &[(DepSpec, Vec<String>)]) -> Vec<HashMap<DepSpec, String>> {
    let mut strategies = vec![HashMap::new()];
    for (spec, versions) in possibilities.iter().rev() {
        let mut extended = Vec::with_capacity(strategies.len() * versions.len());
        for version in versions {
            for strategy in &strategies {
                let mut strategy: HashMap<DepSpec, String> = strategy.clone();
                strategy.insert(spec.clone(), version.clone());
                extended.push(strategy);
            }
        }
        strategies = extended;
    }
    strategies
}

/// Walks the graph from the root, as a strategy resolves its dependencies,
/// checking that the result is consistent.
struct Walker<'a> {
    graph: &'a HashMap<ModuleKey, Interim>,
    groups: &'a HashMap<ModuleKey, Group>,
    multiple: &'a MultipleVersions,
}

impl Walker<'_> {
    fn walk(&self, strategy: &HashMap<DepSpec, String>, ignore_nodeps: bool) -> Result<Vec<Selected>, String> {
        let resolve = |spec: &DepSpec| ModuleKey {
            name: spec.name.clone(),
            version: strategy[spec].clone(),
        };
        let mut by_name: HashMap<String, (ModuleKey, i64, Option<ModuleKey>)> = HashMap::new();
        let mut walked = Vec::new();
        let mut known: HashSet<ModuleKey> = HashSet::from([ModuleKey::root()]);
        let mut to_visit: VecDeque<(ModuleKey, Option<ModuleKey>)> = VecDeque::from([(ModuleKey::root(), None)]);
        while let Some((key, from)) = to_visit.pop_front() {
            let module = &self.graph[&key];
            let deps: Vec<(String, ModuleKey)> = module.deps.iter().map(|(repo, spec)| (repo.clone(), resolve(spec))).collect();
            self.visit(&key, module, from, &deps, &mut by_name)?;
            let nodeps = module.nodep_deps.iter().filter(|_| !ignore_nodeps).map(resolve);
            for dep in deps.iter().map(|(_, dep)| dep.clone()).chain(nodeps) {
                if known.insert(dep.clone()) {
                    to_visit.push_back((dep, Some(key.clone())));
                }
            }
            walked.push(Selected { key, deps });
        }
        Ok(walked)
    }

    fn visit(
        &self,
        key: &ModuleKey,
        module: &Interim,
        from: Option<ModuleKey>,
        deps: &[(String, ModuleKey)],
        by_name: &mut HashMap<String, (ModuleKey, i64, Option<ModuleKey>)>,
    ) -> Result<(), String> {
        let from_shown = from.as_ref().map(ToString::to_string).unwrap_or_default();
        match self.multiple.get(&key.name) {
            Some(allowed) => {
                if self.groups[key].target.is_empty() {
                    return Err(format!(
                        "{from_shown} depends on {key} which is not allowed by the multiple_version_override on {}, which allows only [{}]",
                        key.name,
                        allowed.join(", ")
                    ));
                }
            }
            None => {
                let previous = by_name.insert(key.name.clone(), (key.clone(), module.compatibility_level, from.clone()));
                if let Some((existing, level, dependent)) = previous {
                    let dependent = dependent.map(|dependent| dependent.to_string()).unwrap_or_default();
                    return Err(format!(
                        "{from_shown} depends on {key} with compatibility level {}, but {dependent} depends on {existing} with compatibility level {level} which is different",
                        module.compatibility_level
                    ));
                }
            }
        }
        let mut repos: HashMap<&ModuleKey, &str> = HashMap::new();
        for (repo, dep) in deps {
            if let Some(previous) = repos.insert(dep, repo) {
                return Err(format!(
                    "{key} depends on {dep} at least twice (with repo names {repo} and {previous}). Consider adding a multiple_version_override if you want to depend on multiple versions of {} simultaneously",
                    dep.name
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, version: &str) -> ModuleKey {
        ModuleKey {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    fn dep(name: &str, version: &str) -> (String, DepSpec) {
        let spec = DepSpec {
            name: name.to_string(),
            version: version.to_string(),
            max_compatibility_level: -1,
        };
        (name.to_string(), spec)
    }

    fn module(level: i64, deps: Vec<(String, DepSpec)>) -> Interim {
        Interim {
            compatibility_level: level,
            deps,
            nodep_deps: Vec::new(),
        }
    }

    fn graph(modules: Vec<(ModuleKey, Interim)>) -> (HashMap<ModuleKey, Interim>, Vec<ModuleKey>) {
        let order = modules.iter().map(|(key, _)| key.clone()).collect();
        (modules.into_iter().collect(), order)
    }

    fn shown(selection: &Selection) -> Vec<String> {
        selection.modules.iter().map(|module| module.key.to_string()).collect()
    }

    #[test]
    fn the_highest_version_wanted_is_selected() {
        let (graph, order) = graph(vec![
            (ModuleKey::root(), module(0, vec![dep("a", "1.0"), dep("b", "1.0")])),
            (key("a", "1.0"), module(0, vec![dep("b", "1.1")])),
            (key("b", "1.0"), module(0, vec![])),
            (key("b", "1.1"), module(0, vec![])),
        ]);
        let selection = select(&graph, &order, &MultipleVersions::new()).unwrap();
        assert_eq!(shown(&selection), ["<root>", "a@1.0", "b@1.1"]);
    }

    #[test]
    fn different_compatibility_levels_are_errors() {
        let (graph, order) = graph(vec![
            (ModuleKey::root(), module(0, vec![dep("a", "1.0"), dep("b", "1.0")])),
            (key("a", "1.0"), module(0, vec![dep("b", "2.0")])),
            (key("b", "1.0"), module(1, vec![])),
            (key("b", "2.0"), module(2, vec![])),
        ]);
        let error = select(&graph, &order, &MultipleVersions::new()).err().unwrap();
        assert_eq!(error, "a@1.0 depends on b@2.0 with compatibility level 2, but <root> depends on b@1.0 with compatibility level 1 which is different");
    }

    #[test]
    fn max_compatibility_level_accepts_higher_levels() {
        let (mut graph, order) = graph(vec![
            (ModuleKey::root(), module(0, vec![dep("a", "1.0"), dep("b", "1.0")])),
            (key("a", "1.0"), module(0, vec![dep("b", "2.0")])),
            (key("b", "1.0"), module(1, vec![])),
            (key("b", "2.0"), module(2, vec![])),
        ]);
        graph.get_mut(&ModuleKey::root()).unwrap().deps[1].1.max_compatibility_level = 2;
        let selection = select(&graph, &order, &MultipleVersions::new()).unwrap();
        assert_eq!(shown(&selection), ["<root>", "a@1.0", "b@2.0"]);
    }

    #[test]
    fn multiple_version_overrides_keep_each_allowed_version() {
        let (graph, order) = graph(vec![
            (ModuleKey::root(), module(0, vec![dep("a", "1.0"), dep("b", "1.0")])),
            (key("a", "1.0"), module(0, vec![dep("b", "2.0")])),
            (key("b", "1.0"), module(0, vec![])),
            (key("b", "2.0"), module(0, vec![])),
        ]);
        let multiple = MultipleVersions::from([("b".to_string(), vec!["1.0".to_string(), "2.0".to_string()])]);
        let selection = select(&graph, &order, &multiple).unwrap();
        assert_eq!(shown(&selection), ["<root>", "a@1.0", "b@1.0", "b@2.0"]);
    }

    #[test]
    fn versions_a_multiple_version_override_disallows_are_errors() {
        let (graph, order) = graph(vec![
            (ModuleKey::root(), module(0, vec![dep("a", "1.0"), dep("b", "1.0")])),
            (key("a", "1.0"), module(0, vec![dep("b", "3.0")])),
            (key("b", "1.0"), module(0, vec![])),
            (key("b", "3.0"), module(0, vec![])),
        ]);
        let multiple = MultipleVersions::from([("b".to_string(), vec!["1.0".to_string()])]);
        let error = select(&graph, &order, &multiple).err().unwrap();
        assert_eq!(error, "a@1.0 depends on b@3.0 which is not allowed by the multiple_version_override on b, which allows only [1.0]");
    }
}
