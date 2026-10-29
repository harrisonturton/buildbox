//! Loading `.bzl` files of one repository straight from a directory that
//! never changes, such as Bazel's own `@_builtins` and `@bazel_tools`,
//! without the builtins injected into other `.bzl` files.

use bazel_label::{Label, LabelContext, RepoMappingTable};
use bazel_starlark::{BZL_GLOBALS, EvalContext, FileKind, Loads};
use intern::PathId;
use skycodec::Fingerprint;
use starlark::environment::FrozenModule;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Loads the `.bzl` files of the repository `mapping` is the view of from
/// `dir`, each once. The files are read from disk, not through the graph,
/// so they must not change while the server runs. Blocks.
pub struct DirLoader {
    /// What the modules' heaps are named for, which tells them apart from
    /// other loaders' when they're serialized.
    what: &'static str,
    dir: PathBuf,
    mapping: Arc<RepoMappingTable>,
    modules: RefCell<HashMap<Label, FrozenModule>>,
}

impl DirLoader {
    pub fn new(what: &'static str, dir: PathBuf, mapping: RepoMappingTable) -> DirLoader {
        DirLoader {
            what,
            dir,
            mapping: Arc::new(mapping),
            modules: RefCell::new(HashMap::new()),
        }
    }

    /// The module `label` names, with the modules it loads, which must all
    /// be in the same repository.
    pub fn load(&self, label: Label) -> anyhow::Result<FrozenModule> {
        if let Some(module) = self.modules.borrow().get(&label) {
            return Ok(module.clone());
        }
        let path = self.dir.join(label.package.path.join_path(PathId::parse(label.name.as_str())?).to_string());
        let source = std::fs::read_to_string(&path).map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))?;
        let name = bazel_starlark::heap_name(self.what, label, Fingerprint::of(source.as_bytes()));
        let ast = bazel_starlark::parse(label, source)?;
        let context = LabelContext {
            package: label.package,
            mapping: &*self.mapping,
            lenient: false,
        };
        let mut loaded = Vec::new();
        for load in bazel_starlark::loads(&ast) {
            let target = Label::parse(&load, &context)?;
            if target.repo() != self.mapping.repo {
                anyhow::bail!("{label}: cannot load '{load}' from another repository here");
            }
            loaded.push((load, self.load(target)?));
        }
        let loads = Loads {
            modules: loaded.iter().map(|(load, module)| (load.clone(), module)).collect(),
        };
        let mut eval_context = EvalContext::new(FileKind::Bzl, label);
        eval_context.mappings.insert(self.mapping.repo, self.mapping.clone());
        let module = bazel_starlark::eval_bzl(ast, &BZL_GLOBALS, &loads, eval_context, name).0?;
        self.modules.borrow_mut().insert(label, module.clone());
        Ok(module)
    }
}
