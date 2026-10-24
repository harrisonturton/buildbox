//! Bazel's `@_builtins`: rules such as `cc_library`, and values such as
//! `cc_common`, that Bazel defines in Starlark and gives every BUILD and
//! `.bzl` file.

use crate::blocking;
use bazel_label::{Label, LabelContext, RepoMappingTable, RepoName};
use bazel_packages::RuleClass;
use bazel_starlark::rule_value::StarlarkRule;
use bazel_starlark::{BZL_GLOBALS, EvalContext, FileKind, Loads, StarlarkBuiltins};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{Ctx, Key, SkyError, Storage, Value};
use starlark::environment::{FrozenModule, Globals};
use starlark::values::dict::DictRef;
use starlark::values::FrozenValue;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The repository builtins' files are in.
pub const BUILTINS_REPO: &str = "_builtins";

/// Top-level values builtins export that Bazel 8 doesn't give `.bzl` files
/// by default, since they moved to rule sets.
const HIDDEN_TOPLEVELS: [&str; 6] = ["JavaInfo", "JavaPluginInfo", "java_common", "py_internal", "_builtins_dummy", "proto_common_do_not_use"];

/// A graph service: the directory of Bazel's `builtins_bzl` files, if one
/// was found.
pub struct BuiltinsDir(pub Option<PathBuf>);

/// The globals BUILD and `.bzl` files see, with builtins, and the schemas
/// of the builtin rules.
pub struct Builtins {
    pub bzl: Globals,
    pub build: Globals,
    pub rule_classes: HashMap<Symbol, Arc<RuleClass>>,
    fingerprint: Fingerprint,
    _exports: Option<FrozenModule>,
}

impl Value for Builtins {
    fn to_storage(&self) -> Storage {
        Storage::Live {
            fingerprint: self.fingerprint,
            size: 0,
        }
    }

    fn from_bytes(_bytes: &[u8]) -> Result<Builtins, skycodec::DecodeError> {
        Err(skycodec::DecodeError::Invalid {
            what: "live builtins",
            value: 0,
            position: 0,
        })
    }
}

/// Evaluates the builtins, once per server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BuiltinsKey;

impl Key for BuiltinsKey {
    type Value = Builtins;
    const KIND: &'static str = "BUILTINS";

    async fn compute(&self, ctx: &Ctx) -> Result<Builtins, SkyError> {
        let dir = ctx.global::<BuiltinsDir>()?.0.clone();
        let Some(dir) = dir else {
            return Ok(Builtins {
                bzl: BZL_GLOBALS.clone(),
                build: bazel_starlark::BUILD_GLOBALS.clone(),
                rule_classes: HashMap::new(),
                fingerprint: Fingerprint::of(b"no builtins"),
                _exports: None,
            });
        };
        let mapping = ctx.compute(&crate::RepoMappingKey(RepoName::new(BUILTINS_REPO))).await?.0.clone();
        let span = tracing::info_span!("loading.builtins");
        let builtins = blocking(move || {
            let _entered = span.enter();
            evaluate(&dir, mapping)
        })
        .await?;
        builtins.map_err(|error| SkyError::new(format!("evaluating @_builtins: {error}")))
    }
}

fn evaluate(dir: &Path, mapping: RepoMappingTable) -> anyhow::Result<Builtins> {
    let loader = Loader {
        dir: dir.to_path_buf(),
        mapping: Arc::new(mapping),
        modules: RefCell::new(HashMap::new()),
    };
    let exports = loader.load(builtins_label("exports.bzl")?)?;
    let rules = dict_entries(&exports, "exported_rules")?;
    let toplevels: Vec<(String, FrozenValue)> = dict_entries(&exports, "exported_toplevels")?
        .into_iter()
        .filter(|(name, _)| !HIDDEN_TOPLEVELS.contains(&name.as_str()))
        .collect();

    let mut rule_classes = HashMap::new();
    for (name, value) in &rules {
        let Some(rule) = StarlarkRule::from_value(value.to_value()) else {
            continue;
        };
        if let Some(class) = rule.exported.get() {
            rule_classes.insert(Symbol::intern(name), class.clone());
        }
    }
    let starlark_builtins = StarlarkBuiltins {
        rules,
        toplevels,
        heap: exports.frozen_heap(),
    };
    Ok(Builtins {
        bzl: bazel_starlark::bzl_globals(&starlark_builtins),
        build: bazel_starlark::build_globals(&starlark_builtins),
        rule_classes,
        fingerprint: Fingerprint::of(dir.display().to_string().as_bytes()),
        _exports: Some(exports),
    })
}

/// The entries of a dict a module exports.
fn dict_entries(module: &FrozenModule, name: &str) -> anyhow::Result<Vec<(String, FrozenValue)>> {
    let value = module.get(name).map_err(|_| anyhow::anyhow!("exports.bzl doesn't define {name}"))?;
    let Some(dict) = DictRef::from_value(value.value()) else {
        anyhow::bail!("{name} isn't a dict");
    };
    let mut entries = Vec::new();
    for (key, value) in dict.iter() {
        let (Some(key), Some(value)) = (key.unpack_str(), value.unpack_frozen()) else {
            continue;
        };
        entries.push((key.to_string(), value));
    }
    Ok(entries)
}

fn builtins_label(path: &str) -> anyhow::Result<Label> {
    Ok(Label::parse_canonical(&format!("@@{BUILTINS_REPO}//:{path}"))?)
}

/// How builtins see other repositories: as `@bazel_tools` does, and
/// themselves as `@_builtins`.
pub fn builtins_mapping(bazel_tools: &RepoMappingTable) -> RepoMappingTable {
    let mut entries = bazel_tools.entries.clone();
    entries.push((Symbol::intern(BUILTINS_REPO), RepoName::new(BUILTINS_REPO)));
    RepoMappingTable::new(RepoName::new(BUILTINS_REPO), entries)
}

/// Loads builtins' `.bzl` files straight from disk: they never change, so
/// they don't need keys of their own.
struct Loader {
    dir: PathBuf,
    mapping: Arc<RepoMappingTable>,
    modules: RefCell<HashMap<Label, FrozenModule>>,
}

impl Loader {
    fn load(&self, label: Label) -> anyhow::Result<FrozenModule> {
        if let Some(module) = self.modules.borrow().get(&label) {
            return Ok(module.clone());
        }
        let path = self.dir.join(label.package.path.join_path(PathId::parse(label.name.as_str())?).to_string());
        let source = std::fs::read_to_string(&path).map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))?;
        let ast = bazel_starlark::parse(label, source)?;
        let context = LabelContext {
            package: label.package,
            mapping: &*self.mapping,
            lenient: false,
        };
        let mut loaded = Vec::new();
        for load in bazel_starlark::loads(&ast) {
            let target = Label::parse(&load, &context)?;
            loaded.push((load, self.load(target)?));
        }
        let loads = Loads {
            modules: loaded.iter().map(|(load, module)| (load.clone(), module)).collect(),
        };
        let mut eval_context = EvalContext::new(FileKind::Bzl, label);
        eval_context.mappings.insert(RepoName::new(BUILTINS_REPO), self.mapping.clone());
        let (module, _) = bazel_starlark::eval_bzl(ast, &BZL_GLOBALS, &loads, eval_context)?;
        self.modules.borrow_mut().insert(label, module.clone());
        Ok(module)
    }
}
