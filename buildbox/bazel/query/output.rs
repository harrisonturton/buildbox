//! Printing query results in Bazel's output formats.

use crate::eval::{edges, kind_of};
use crate::universe::{QueryError, TargetRef, TargetSet, Universe};
use bazel_fs::Root;
use bazel_label::{Label, PackageId, RepoMappingTable};
use bazel_loading::RepoRootKey;
use bazel_packages::{AttrValue, Rule, SelectorPart, Target};
use skyframe::Compute;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// How query results are printed, as `--output` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Label,
    LabelKind,
    Package,
    Location,
    Build,
    MinRank,
    MaxRank,
    Graph,
}

impl OutputFormat {
    pub fn parse(name: &str) -> Option<OutputFormat> {
        Some(match name {
            "label" => OutputFormat::Label,
            "label_kind" => OutputFormat::LabelKind,
            "package" => OutputFormat::Package,
            "location" => OutputFormat::Location,
            "build" => OutputFormat::Build,
            "minrank" => OutputFormat::MinRank,
            "maxrank" => OutputFormat::MaxRank,
            "graph" => OutputFormat::Graph,
            _ => return None,
        })
    }
}

/// A label as Bazel prints it: `//pkg:name` in the main repository,
/// `@name//pkg:name` for repositories the main repository can see, and
/// `@@canonical//pkg:name` for the rest.
pub fn display_label(label: Label, mapping: &RepoMappingTable) -> String {
    format!("{}:{}", display_package(label.package, mapping), label.name)
}

pub fn display_package(package: PackageId, mapping: &RepoMappingTable) -> String {
    if package.repo.is_main() {
        return format!("//{}", package.path);
    }
    match mapping.apparent_name(package.repo) {
        Some(apparent) if !apparent.is_empty() => format!("@{apparent}//{}", package.path),
        _ => format!("@@{}//{}", package.repo, package.path),
    }
}

/// Print `targets` in `format`.
pub async fn format<C: Compute>(
    universe: &Universe<C>,
    targets: &TargetSet,
    format: OutputFormat,
    workspace_root: &Path,
) -> Result<String, QueryError> {
    let mapping = universe.main_mapping.clone();
    let mut out = String::new();
    match format {
        OutputFormat::Label => {
            for label in targets {
                writeln!(out, "{}", display_label(*label, &mapping)).ok();
            }
        }
        OutputFormat::LabelKind => {
            for target in universe.targets(targets).await? {
                writeln!(out, "{} {}", kind_of(&target), display_label(target.label(), &mapping)).ok();
            }
        }
        OutputFormat::Package => {
            let packages: BTreeSet<PackageId> = targets.iter().map(|label| label.package).collect();
            for package in packages {
                let display = display_package(package, &mapping);
                writeln!(out, "{}", display.strip_prefix("//").unwrap_or(&display)).ok();
            }
        }
        OutputFormat::Location => {
            let roots = RepoPaths::new(universe, workspace_root);
            for target in universe.targets(targets).await? {
                let location = roots.location(&target).await?;
                writeln!(out, "{location}: {} {}", kind_of(&target), display_label(target.label(), &mapping)).ok();
            }
        }
        OutputFormat::Build => {
            let roots = RepoPaths::new(universe, workspace_root);
            for target in universe.targets(targets).await? {
                if let Target::Rule(rule) = target.target() {
                    let location = roots.location(&target).await?;
                    write_build(&mut out, &target, rule, &location, &mapping);
                }
            }
        }
        OutputFormat::MinRank | OutputFormat::MaxRank => {
            let ranks = ranks(universe, targets, format == OutputFormat::MaxRank).await?;
            let mut ranked: Vec<(usize, Label)> = ranks.into_iter().map(|(label, rank)| (rank, label)).collect();
            ranked.sort();
            for (rank, label) in ranked {
                writeln!(out, "{rank} {}", display_label(label, &mapping)).ok();
            }
        }
        OutputFormat::Graph => {
            let edges = edges(universe, targets).await?;
            writeln!(out, "digraph mygraph {{\n  node [shape=box];").ok();
            for label in targets {
                let name = display_label(*label, &mapping);
                writeln!(out, "  \"{name}\"").ok();
                for dep in edges.get(label).into_iter().flatten().filter(|dep| targets.contains(dep)) {
                    writeln!(out, "  \"{name}\" -> \"{}\"", display_label(*dep, &mapping)).ok();
                }
            }
            writeln!(out, "}}").ok();
        }
    }
    Ok(out)
}

/// Each target's rank: the shortest (or, for maxrank, longest) path to it
/// from a target nothing else in the set depends on.
async fn ranks<C: Compute>(universe: &Universe<C>, targets: &TargetSet, longest: bool) -> Result<HashMap<Label, usize>, QueryError> {
    let edges = edges(universe, targets).await?;
    let mut indegree: HashMap<Label, usize> = targets.iter().map(|label| (*label, 0)).collect();
    for label in targets {
        for dep in edges.get(label).into_iter().flatten().filter(|dep| targets.contains(dep)) {
            *indegree.entry(*dep).or_default() += 1;
        }
    }
    let mut ranks: HashMap<Label, usize> = HashMap::new();
    let mut ready: Vec<Label> = indegree.iter().filter(|(_, count)| **count == 0).map(|(label, _)| *label).collect();
    for label in &ready {
        ranks.insert(*label, 0);
    }
    while let Some(label) = ready.pop() {
        let rank = ranks[&label];
        for dep in edges.get(&label).into_iter().flatten().filter(|dep| targets.contains(dep)) {
            let entry = ranks.entry(*dep).or_insert(if longest { 0 } else { usize::MAX });
            *entry = if longest { (*entry).max(rank + 1) } else { (*entry).min(rank + 1) };
            let count = indegree.get_mut(dep).expect("every target has an in-degree");
            *count -= 1;
            if *count == 0 {
                ready.push(*dep);
            }
        }
    }
    for label in targets {
        ranks.entry(*label).or_insert(0);
    }
    Ok(ranks)
}

/// Resolves repositories to directories, for printing file locations.
struct RepoPaths<'a, C> {
    universe: &'a Universe<C>,
    workspace_root: &'a Path,
}

impl<'a, C: Compute> RepoPaths<'a, C> {
    fn new(universe: &'a Universe<C>, workspace_root: &'a Path) -> RepoPaths<'a, C> {
        RepoPaths { universe, workspace_root }
    }

    async fn package_dir(&self, package: PackageId) -> Result<PathBuf, QueryError> {
        let root = self.universe.compute().compute(&RepoRootKey(package.repo)).await?;
        let base = match root.0 {
            Root::Workspace => self.workspace_root.to_path_buf(),
            Root::Dir(dir) => PathBuf::from(dir.as_str()),
        };
        Ok(if package.path.is_root() { base } else { base.join(package.path.to_string()) })
    }

    /// Where a target is defined: `path:line:column`.
    async fn location(&self, target: &TargetRef) -> Result<String, QueryError> {
        let package = target.package();
        let dir = self.package_dir(package.id).await?;
        let build_file = dir.join(package.build_file.name.as_str());
        let location = match target.target() {
            Target::Rule(rule) => format!("{}:{}:{}", build_file.display(), rule.location.line, rule.location.column),
            Target::OutputFile { rule, .. } => match package.target(*rule).and_then(Target::as_rule) {
                Some(rule) => format!("{}:{}:{}", build_file.display(), rule.location.line, rule.location.column),
                None => format!("{}:1:1", build_file.display()),
            },
            Target::SourceFile(file) if file.name == package.build_file.name => format!("{}:1:1", build_file.display()),
            Target::SourceFile(file) => format!("{}:1:1", dir.join(file.name.as_str()).display()),
            Target::PackageGroup(_) => format!("{}:1:1", build_file.display()),
        };
        Ok(location)
    }
}

fn write_build(out: &mut String, target: &TargetRef, rule: &Rule, location: &str, mapping: &RepoMappingTable) {
    let kind = target.package().rule_class(rule).kind;
    writeln!(out, "# {location}").ok();
    writeln!(out, "{kind}(").ok();
    writeln!(out, "  name = \"{}\",", rule.name).ok();
    for (attr, value) in &rule.attrs {
        writeln!(out, "  {attr} = {},", build_value(value, mapping)).ok();
    }
    writeln!(out, ")").ok();
    writeln!(out, "# Rule {} instantiated at (most recent call last):", rule.name).ok();
    writeln!(out, "#   {location} in <toplevel>").ok();
}

fn build_value(value: &AttrValue, mapping: &RepoMappingTable) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"));
    let label = |l: &Label| quote(&display_label(*l, mapping));
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    let dict = |entries: Vec<(String, String)>| {
        format!("{{{}}}", entries.into_iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", "))
    };
    match value {
        AttrValue::None => "None".to_string(),
        AttrValue::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        AttrValue::Int(i) => i.to_string(),
        AttrValue::String(s) => quote(s),
        AttrValue::Label(l) => label(l),
        AttrValue::StringList(items) => list(items.iter().map(|s| quote(s)).collect()),
        AttrValue::LabelList(labels) => list(labels.iter().map(label).collect()),
        AttrValue::IntList(items) => list(items.iter().map(i64::to_string).collect()),
        AttrValue::StringDict(entries) => dict(entries.iter().map(|(k, v)| (quote(k), quote(v))).collect()),
        AttrValue::StringListDict(entries) => {
            dict(entries.iter().map(|(k, v)| (quote(k), list(v.iter().map(|s| quote(s)).collect()))).collect())
        }
        AttrValue::LabelKeyedStringDict(entries) => dict(entries.iter().map(|(k, v)| (label(k), quote(v))).collect()),
        AttrValue::StringKeyedLabelDict(entries) => dict(entries.iter().map(|(k, v)| (quote(k), label(v))).collect()),
        AttrValue::Configurable(parts) => parts
            .iter()
            .map(|part| match part {
                SelectorPart::Value(value) => build_value(value, mapping),
                SelectorPart::Select(select) => {
                    let branches = select
                        .branches
                        .iter()
                        .map(|(condition, value)| {
                            let condition = if condition.to_string() == bazel_packages::DEFAULT_CONDITION {
                                quote(bazel_packages::DEFAULT_CONDITION)
                            } else {
                                label(condition)
                            };
                            (condition, build_value(value, mapping))
                        })
                        .collect();
                    format!("select({})", dict(branches))
                }
            })
            .collect::<Vec<_>>()
            .join(" + "),
    }
}
