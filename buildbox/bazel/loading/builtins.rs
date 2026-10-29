//! Bazel's `@_builtins`: rules such as `cc_library`, and values such as
//! `cc_common`, that Bazel defines in Starlark and gives every BUILD and
//! `.bzl` file.

use crate::blocking;
use crate::dir_loader::DirLoader;
use bazel_label::{Label, RepoMappingTable, RepoName};
use bazel_packages::RuleClass;
use bazel_starlark::rule_value::StarlarkRule;
use bazel_starlark::{BZL_GLOBALS, StarlarkBuiltins};
use intern::Symbol;
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{Ctx, Key, SkyError, Storage, Value};
use starlark::environment::{FrozenModule, Globals};
use starlark::values::dict::DictRef;
use starlark::values::{FrozenValue, OwnedFrozenValue, ValueLike};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The repository builtins' files are in.
pub const BUILTINS_REPO: &str = "_builtins";

/// Top-level values builtins export that Bazel 8 doesn't give `.bzl` files
/// by default, since rule sets define their own.
const HIDDEN_TOPLEVELS: [&str; 3] = ["JavaInfo", "JavaPluginInfo", "_builtins_dummy"];

/// A graph service: the directory of Bazel's `builtins_bzl` files, if one
/// was found.
pub struct BuiltinsDir(pub Option<PathBuf>);

/// The globals BUILD and `.bzl` files see, with builtins, and the schemas
/// of the builtin rules.
pub struct Builtins {
    pub bzl: Globals,
    pub build: Globals,
    pub rule_classes: HashMap<Symbol, Arc<RuleClass>>,
    /// The transition to the exec configuration, which Bazel's builtins
    /// define in Starlark.
    pub exec_transition: Option<OwnedFrozenValue>,
    fingerprint: Fingerprint,
    /// The rules and top-level values builtins export, in `_exports`'s
    /// heap.
    rules: Vec<(String, FrozenValue)>,
    toplevels: Vec<(String, FrozenValue)>,
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
                exec_transition: None,
                fingerprint: Fingerprint::of(b"no builtins"),
                rules: Vec::new(),
                toplevels: Vec::new(),
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
    let loader = DirLoader::new("builtins", dir.to_path_buf(), mapping);
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
    let exec_platforms = loader.load(builtins_label("common/builtin_exec_platforms.bzl")?)?;
    let exec_transition = exec_platforms
        .get("bazel_exec_transition")
        .map_err(|_| anyhow::anyhow!("common/builtin_exec_platforms.bzl doesn't define bazel_exec_transition"))?;
    let fingerprint = Fingerprint::of(dir.display().to_string().as_bytes());
    let starlark_builtins = StarlarkBuiltins {
        rules: rules.clone(),
        toplevels: toplevels.clone(),
        heap: exports.frozen_heap(),
        id: fingerprint.to_string(),
    };
    Ok(Builtins {
        bzl: bazel_starlark::bzl_globals(&starlark_builtins),
        build: bazel_starlark::build_globals(&starlark_builtins),
        rule_classes,
        exec_transition: Some(exec_transition),
        fingerprint,
        rules,
        toplevels,
        _exports: Some(exports),
    })
}

impl Builtins {
    /// What builtins export, for making globals with other changes, or
    /// `None` if there are no builtins.
    pub fn starlark_builtins(&self) -> Option<StarlarkBuiltins<'_>> {
        let exports = self._exports.as_ref()?;
        Some(StarlarkBuiltins {
            rules: self.rules.clone(),
            toplevels: self.toplevels.clone(),
            heap: exports.frozen_heap(),
            id: self.fingerprint.to_string(),
        })
    }

    /// The implementation function of a builtin rule, by name.
    pub fn rule_implementation(&self, kind: &str) -> Option<OwnedFrozenValue> {
        let rules = self._exports.as_ref()?.get("exported_rules").ok()?;
        rules
            .try_map(|rules| {
                let rule = DictRef::from_value(rules.to_value()).and_then(|rules| rules.get_str(kind)).ok_or(())?;
                let rule = StarlarkRule::from_value(rule).ok_or(())?;
                rule.implementation.to_value().unpack_frozen().ok_or(())
            })
            .ok()
    }
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
