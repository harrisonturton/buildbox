//! The configured graph `cquery` and `aquery` read: the targets analysis
//! configured from the universe's top-level targets, and the dependencies
//! analysis followed between them.

use crate::node::ConfiguredNode;
use async_trait::async_trait;
use bazel_analysis::{ConfiguredAttrsKey, ConfiguredTarget, ConfiguredTargetKey};
use bazel_config::ConfigId;
use bazel_label::{Label, RepoName};
use bazel_packages::{AttrValue, RuleClass, RuleClassRef};
use bazel_query::{
    AQUERY_FUNCTIONS, Arg, CQUERY_FUNCTIONS, Context, DEFAULT_FUNCTIONS, Function, NodeSet, QueryEnvironment, QueryError, TargetRef, Universe, attr_string, expr_arg,
    possible_values, word_arg,
};
use intern::Symbol;
use skyframe::Compute;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// Which dependencies a configured query follows.
#[derive(Clone, Copy, Debug)]
pub struct ConfiguredOptions {
    /// Follow dependencies the BUILD file doesn't name.
    pub implicit_deps: bool,
    /// Follow dependencies built for the execution platform.
    pub tool_deps: bool,
    /// Include what aspects applied to targets' dependencies depend on and
    /// do, `--include_aspects`.
    pub include_aspects: bool,
}

/// The query language a configured graph is queried in: `cquery`'s, with
/// `config()`, or `aquery`'s, with functions that filter actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Cquery,
    Aquery,
}

impl Language {
    /// The functions queries in the language can call.
    pub fn functions(self) -> Vec<Function> {
        let mut functions = DEFAULT_FUNCTIONS.to_vec();
        match self {
            Language::Cquery => functions.extend_from_slice(CQUERY_FUNCTIONS),
            Language::Aquery => functions.extend_from_slice(AQUERY_FUNCTIONS),
        }
        functions
    }
}

/// The configured targets of a universe.
pub struct ConfiguredGraph<C> {
    /// Loads the packages targets are in, and resolves patterns.
    pub packages: Universe<C>,
    pub options: ConfiguredOptions,
    pub language: Language,
    /// The configuration top-level targets are built in.
    pub top_level_config: ConfigId,
    /// The universe's top-level targets, as analyzed.
    roots: Vec<ConfiguredNode>,
    /// The applications of `--aspects` to the top-level targets.
    top_level_aspects: Vec<ConfiguredTargetKey>,
    /// The universe's nodes, by label, once they're needed.
    nodes: OnceLock<HashMap<Label, Vec<ConfiguredNode>>>,
    targets: Mutex<HashMap<ConfiguredTargetKey, Option<Arc<ConfiguredTarget>>>>,
}

impl<C: Compute> ConfiguredGraph<C> {
    pub fn new(
        packages: Universe<C>,
        options: ConfiguredOptions,
        language: Language,
        top_level_config: ConfigId,
        roots: Vec<ConfiguredNode>,
        top_level_aspects: Vec<ConfiguredTargetKey>,
    ) -> ConfiguredGraph<C> {
        ConfiguredGraph {
            packages,
            options,
            language,
            top_level_config,
            roots,
            top_level_aspects,
            nodes: OnceLock::new(),
            targets: Mutex::new(HashMap::new()),
        }
    }

    /// The configured target a node is, if it's one analysis configured.
    pub async fn configured(&self, node: &ConfiguredNode) -> Option<Arc<ConfiguredTarget>> {
        let key = node.key()?;
        if let Some(found) = self.targets.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
            return found.clone();
        }
        let target = self.packages.compute().compute(&key).await.ok();
        self.targets.lock().unwrap_or_else(|p| p.into_inner()).insert(key, target.clone());
        target
    }

    /// The nodes of the universe: its top-level targets and every target
    /// they depend on, by label.
    async fn universe(&self) -> &HashMap<Label, Vec<ConfiguredNode>> {
        if let Some(nodes) = self.nodes.get() {
            return nodes;
        }
        let mut seen: NodeSet<ConfiguredNode> = self.roots.iter().copied().collect();
        let mut pending: VecDeque<ConfiguredNode> = self.roots.iter().copied().collect();
        while let Some(node) = pending.pop_front() {
            for dep in self.all_deps(&node).await {
                let dep = ConfiguredNode { label: dep.label, config: dep.config };
                if seen.insert(dep) {
                    pending.push_back(dep);
                }
            }
        }
        let mut nodes: HashMap<Label, Vec<ConfiguredNode>> = HashMap::new();
        for node in seen {
            nodes.entry(node.label).or_default().push(node);
        }
        for configured in nodes.values_mut() {
            configured.sort();
        }
        self.nodes.get_or_init(|| nodes)
    }

    /// A node's dependencies, and with `--include_aspects`, those of the
    /// aspects applied to its dependencies, which Bazel counts as its own.
    async fn all_deps(&self, node: &ConfiguredNode) -> Vec<bazel_analysis::ConfiguredDep> {
        let Some(target) = self.configured(node).await else {
            return Vec::new();
        };
        let mut deps = target.deps.clone();
        if !self.options.include_aspects {
            return deps;
        }
        for application in &target.aspects {
            let application = ConfiguredNode { label: application.label, config: Some(application.config) };
            let Some(applied) = self.configured(&application).await else {
                continue;
            };
            for dep in &applied.deps {
                if !deps.contains(dep) {
                    deps.push(*dep);
                }
            }
        }
        deps
    }

    /// The repositories of a node and of everything it depends on,
    /// transitively: those whose packages its analysis loaded.
    pub async fn transitive_repos(&self, node: &ConfiguredNode) -> BTreeSet<RepoName> {
        let mut repos = BTreeSet::new();
        let mut visited: HashSet<ConfiguredNode> = HashSet::new();
        let mut pending = vec![*node];
        while let Some(node) = pending.pop() {
            if !visited.insert(node) {
                continue;
            }
            repos.insert(node.label.repo());
            if node.config.is_none() {
                continue;
            }
            for dep in self.all_deps(&node).await {
                pending.push(ConfiguredNode { label: dep.label, config: dep.config });
            }
        }
        repos
    }

    /// The applications of aspects to each target of the universe, by the
    /// target, as `aquery` lists their actions: those its dependents
    /// applied, transitively, and `--aspects`'.
    pub async fn aspect_applications(&self) -> HashMap<ConfiguredTargetKey, Vec<ConfiguredTargetKey>> {
        let mut pending: Vec<ConfiguredTargetKey> = self.top_level_aspects.clone();
        for node in self.universe().await.values().flatten() {
            if let Some(target) = self.configured(node).await {
                pending.extend(target.aspects.iter().copied());
            }
        }
        let mut seen = std::collections::HashSet::new();
        let mut applications: HashMap<ConfiguredTargetKey, Vec<ConfiguredTargetKey>> = HashMap::new();
        while let Some(application) = pending.pop() {
            if !seen.insert(application) {
                continue;
            }
            let base = ConfiguredTargetKey { label: application.label, config: application.config.get().without_aspects() };
            applications.entry(base).or_default().push(application);
            let node = ConfiguredNode { label: application.label, config: Some(application.config) };
            if let Some(applied) = self.configured(&node).await {
                pending.extend(applied.aspects.iter().copied());
            }
        }
        for keys in applications.values_mut() {
            keys.sort();
        }
        applications
    }

    /// Whether a dependency is followed, as the options say: a tool, built
    /// for the execution platform, only from another tool.
    fn follows(&self, from: &ConfiguredNode, dep: &bazel_analysis::ConfiguredDep) -> bool {
        if !self.options.implicit_deps && dep.implicit {
            return false;
        }
        if self.options.tool_deps {
            return true;
        }
        let is_exec = |config: Option<ConfigId>| config.is_some_and(|config| config.get().is_exec);
        match is_exec(from.config) {
            true => is_exec(dep.config),
            false => !is_exec(dep.config),
        }
    }

    /// The configuration `config()`'s second argument names: `target`,
    /// `null`, or a configuration's checksum or a prefix of it.
    async fn named_config(&self, name: &str) -> Result<Option<ConfigId>, QueryError> {
        match name {
            "target" => return Ok(Some(self.top_level_config)),
            "null" => return Ok(None),
            _ => {}
        }
        let mut found: Vec<ConfigId> = Vec::new();
        for node in self.universe().await.values().flatten() {
            let Some(config) = node.config else {
                continue;
            };
            if config.get().digest.starts_with(name) && !found.contains(&config) {
                found.push(config);
            }
        }
        match found.as_slice() {
            [config] => Ok(Some(*config)),
            _ => Err(QueryError::in_evaluation(unknown_config(name))),
        }
    }
}

/// The error for `config()` naming no single configuration, as Bazel's.
fn unknown_config(name: &str) -> String {
    format!(
        "Unknown configuration ID '{name}'.\nconfig()'s second argument must identify a unique configuration.\n\nValid values:\n 'target' for the default configuration\n 'null' for source files (which have no configuration)\n an arbitrary configuration's full or short ID\n\nA short ID is any prefix of a full ID. cquery shows short IDs. 'bazel config' shows full IDs.\n\nFor more help, see https://bazel.build/docs/cquery."
    )
}

#[async_trait]
impl<C: Compute> QueryEnvironment for ConfiguredGraph<C> {
    type Node = ConfiguredNode;

    fn functions(&self) -> Vec<Function> {
        self.language.functions()
    }

    fn label(&self, node: &ConfiguredNode) -> Label {
        node.label
    }

    async fn targets_matching(&self, pattern: &str) -> Result<NodeSet<ConfiguredNode>, QueryError> {
        let labels = self.packages.resolve(pattern).await?;
        let universe = self.universe().await;
        let mut nodes = NodeSet::new();
        for label in labels {
            nodes.extend(universe.get(&label).into_iter().flatten().copied());
        }
        Ok(nodes)
    }

    async fn fwd_deps(&self, node: &ConfiguredNode) -> Result<Vec<ConfiguredNode>, QueryError> {
        let deps = self.all_deps(node).await;
        let followed = deps.iter().filter(|dep| self.follows(node, dep));
        Ok(followed.map(|dep| ConfiguredNode { label: dep.label, config: dep.config }).collect())
    }

    async fn target_of(&self, node: &ConfiguredNode) -> Result<TargetRef, QueryError> {
        self.packages.target(node.label).await
    }

    async fn rule_class(&self, class: &RuleClassRef) -> Result<Arc<RuleClass>, QueryError> {
        self.packages.rule_class(class).await
    }

    async fn attr_strings(&self, node: &ConfiguredNode, attr: Symbol) -> Result<Option<Vec<String>>, QueryError> {
        if attr.as_str() == "name" {
            return Ok(Some(vec![node.label.name.to_string()]));
        }
        let Some(value) = self.configured_attr(node, attr).await? else {
            return Ok(None);
        };
        Ok(Some(possible_values(&value).iter().map(attr_string).collect()))
    }

    async fn attr_nodes(&self, node: &ConfiguredNode, attr: Symbol) -> Result<Vec<ConfiguredNode>, QueryError> {
        let Some(value) = self.configured_attr(node, attr).await? else {
            return Ok(Vec::new());
        };
        let mut labels = Vec::new();
        value.labels(&mut labels);
        let Some(target) = self.configured(node).await else {
            return Ok(Vec::new());
        };
        let deps = target.deps.iter().filter(|dep| labels.contains(&dep.label));
        Ok(deps.map(|dep| ConfiguredNode { label: dep.label, config: dep.config }).collect())
    }

    async fn siblings(&self, _nodes: &NodeSet<ConfiguredNode>) -> Result<NodeSet<ConfiguredNode>, QueryError> {
        Err(QueryError::in_expression("siblings() not supported for post analysis queries"))
    }

    async fn is_visible(&self, from: &ConfiguredNode, target: &ConfiguredNode) -> Result<bool, QueryError> {
        self.packages.is_visible(&from.label, &target.label).await
    }

    async fn call(&self, cx: &Context<'_, Self>, name: &str, args: &[Arg]) -> Result<NodeSet<ConfiguredNode>, QueryError> {
        match (self.language, name) {
            (Language::Aquery, "inputs" | "outputs" | "mnemonic") => expr_arg(cx, args, 1).await,
            (Language::Cquery, "config") => {
                let nodes = expr_arg(cx, args, 0).await?;
                let config = self.named_config(word_arg(args, 1)?).await?;
                Ok(nodes.into_iter().filter(|node| node.config == config).collect())
            }
            (_, "buildfiles" | "loadfiles") => Err(QueryError::in_expression(format!("{name}() doesn't make sense for the configured target graph"))),
            _ => Err(QueryError::in_expression(format!("{name}() isn't supported"))),
        }
    }
}

impl<C: Compute> ConfiguredGraph<C> {
    /// An attribute of a node's rule, as configured; `None` for a target
    /// that isn't a rule, or a rule without the attribute.
    pub async fn configured_attr(&self, node: &ConfiguredNode, attr: Symbol) -> Result<Option<AttrValue>, QueryError> {
        let attrs = self.configured_attrs(node).await?;
        Ok(attrs.into_iter().find(|(name, _)| *name == attr).map(|(_, value)| value))
    }

    /// Every attribute of a node's rule, as configured.
    pub async fn configured_attrs(&self, node: &ConfiguredNode) -> Result<Vec<(Symbol, AttrValue)>, QueryError> {
        let Some(key) = node.key() else {
            return Ok(Vec::new());
        };
        let attrs = self.packages.compute().compute(&ConfiguredAttrsKey(key)).await?;
        Ok(attrs.0.clone())
    }
}
