//! Evaluating MODULE.bazel files.

use crate::star_value::StarValue;
use allocative::Allocative;
use skycodec::{Decode, Encode};
use starlark::collections::SmallMap;
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::syntax::AstModule;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, StringValue, Value, ValueLike, starlark_value};
use bazel_packages::PackageError;
use std::cell::RefCell;
use std::fmt;
use std::sync::LazyLock;

/// What a MODULE.bazel file declares.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
pub struct ModuleFile {
    pub name: String,
    pub version: String,
    pub compatibility_level: i64,
    /// The name the module's repository has for itself, if not its name.
    pub repo_name: Option<String>,
    pub deps: Vec<BazelDep>,
    pub extension_usages: Vec<ExtensionUsage>,
    /// Repositories defined with `use_repo_rule`.
    pub repo_rules: Vec<RepoRuleCall>,
    pub toolchains: Vec<String>,
    pub execution_platforms: Vec<String>,
    pub overrides: Vec<(String, Override)>,
    /// The versions of Bazel the module works with, as `module()`'s
    /// `bazel_compatibility` gives them, such as `>=7.0.0`.
    pub bazel_compatibility: Vec<String>,
    /// Warnings about the root module's file: the line and column of the
    /// call each is about, and what it says.
    pub warnings: Vec<(u32, u32, String)>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct BazelDep {
    pub name: String,
    pub version: String,
    pub max_compatibility_level: i64,
    /// The dependency's name in this module, or `None` for a dependency
    /// that only constrains version selection.
    pub repo_name: Option<String>,
    pub dev: bool,
}

/// One module's use of a module extension.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ExtensionUsage {
    /// The `.bzl` file defining the extension, as the module wrote it.
    pub bzl: String,
    pub name: String,
    pub isolate: bool,
    pub imports: Vec<RepoImport>,
    pub tags: Vec<Tag>,
    /// `override_repo` and `inject_repo`: the extension's repository named
    /// by the first replaced by the module's repository named by the second.
    pub repo_overrides: Vec<(String, String)>,
    /// Whether every use of the extension was marked `dev_dependency`.
    pub dev_only: bool,
}

/// A repository an extension generates, imported into the module with
/// `use_repo`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RepoImport {
    /// The name the module uses.
    pub apparent: String,
    /// The name the extension gave it.
    pub exported: String,
    pub dev: bool,
}

/// A tag: `extension.tag_class(attr = value, ...)`.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Tag {
    pub class: String,
    pub attrs: Vec<(String, StarValue)>,
    pub dev: bool,
    pub location: String,
}

/// A repository defined with `use_repo_rule`.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RepoRuleCall {
    pub bzl: String,
    pub rule: String,
    pub name: String,
    pub attrs: Vec<(String, StarValue)>,
    pub dev: bool,
}

/// How the root module replaces another module's source or version.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Override {
    SingleVersion {
        version: String,
        registry: String,
        patches: Vec<String>,
        patch_cmds: Vec<String>,
        patch_strip: i64,
    },
    MultipleVersion {
        versions: Vec<String>,
        registry: String,
    },
    /// An archive, as `http_archive` fetches it.
    Archive {
        urls: Vec<String>,
        /// A subresource integrity, or a SHA-256 in hex.
        integrity: String,
        strip_prefix: String,
        archive_type: String,
        patches: Vec<String>,
        patch_cmds: Vec<String>,
        patch_strip: i64,
    },
    /// A commit of a Git repository, as `git_repository` fetches it.
    Git(GitSource),
    LocalPath {
        path: String,
    },
}

impl Override {
    /// Whether the module comes from somewhere other than a registry.
    pub fn is_non_registry(&self) -> bool {
        matches!(self, Override::Archive { .. } | Override::Git { .. } | Override::LocalPath { .. })
    }
}

/// What to fetch of a Git repository, and how to patch it, as
/// `git_repository`'s attributes say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub struct GitSource {
    pub remote: String,
    pub commit: String,
    pub tag: String,
    pub branch: String,
    pub shallow_since: String,
    pub init_submodules: bool,
    pub recursive_init_submodules: bool,
    pub strip_prefix: String,
    /// Patches, as labels of the main repository.
    pub patches: Vec<String>,
    pub patch_cmds: Vec<String>,
    pub patch_strip: i64,
}

/// The attributes `git_repository` has.
const GIT_REPOSITORY_ATTRS: &[&str] = &[
    "branch",
    "build_file",
    "build_file_content",
    "canonical_id",
    "commit",
    "init_submodules",
    "patch_args",
    "patch_cmds",
    "patch_cmds_win",
    "patch_strip",
    "patch_tool",
    "patches",
    "recursive_init_submodules",
    "remote",
    "remote_module_file_integrity",
    "remote_module_file_urls",
    "remote_patch_strip",
    "remote_patches",
    "shallow_since",
    "sparse_checkout_file",
    "sparse_checkout_patterns",
    "strip_prefix",
    "tag",
    "verbose",
    "workspace_file",
    "workspace_file_content",
];

/// The attributes of `git_repository` bb implements for `git_override`.
/// `patch_cmds_win` and `verbose` change nothing here, as in Bazel off
/// Windows.
const GIT_OVERRIDE_ATTRS: &[&str] = &[
    "branch",
    "canonical_id",
    "commit",
    "init_submodules",
    "patch_args",
    "patch_cmds",
    "patch_cmds_win",
    "patch_strip",
    "patches",
    "recursive_init_submodules",
    "remote",
    "shallow_since",
    "strip_prefix",
    "tag",
    "verbose",
];

/// The attributes `http_archive` has.
const HTTP_ARCHIVE_ATTRS: &[&str] = &[
    "add_prefix",
    "auth_patterns",
    "build_file",
    "build_file_content",
    "canonical_id",
    "files",
    "integrity",
    "netrc",
    "patch_args",
    "patch_cmds",
    "patch_cmds_win",
    "patch_strip",
    "patch_tool",
    "patches",
    "remote_file_integrity",
    "remote_file_urls",
    "remote_module_file_integrity",
    "remote_module_file_urls",
    "remote_patch_strip",
    "remote_patches",
    "sha256",
    "strip_components",
    "strip_prefix",
    "type",
    "url",
    "urls",
    "workspace_file",
    "workspace_file_content",
];

/// The attributes of `http_archive` bb implements for `archive_override`.
const ARCHIVE_OVERRIDE_ATTRS: &[&str] = &[
    "canonical_id",
    "integrity",
    "patch_args",
    "patch_cmds",
    "patch_cmds_win",
    "patch_strip",
    "patches",
    "sha256",
    "strip_prefix",
    "type",
    "url",
    "urls",
];

/// The attributes an override forwards to its repository rule, checked
/// against those the rule has and those bb implements.
struct Forwarded<'v> {
    attrs: SmallMap<StringValue<'v>, Value<'v>>,
}

impl<'v> Forwarded<'v> {
    fn check(attrs: SmallMap<StringValue<'v>, Value<'v>>, function: &str, rule: &str, known: &[&str], supported: &[&str]) -> anyhow::Result<Forwarded<'v>> {
        for name in attrs.keys() {
            let name = name.as_str();
            if !known.contains(&name) {
                anyhow::bail!("in call to '{rule}' repo rule, unknown attribute '{name}' provided");
            }
            if !supported.contains(&name) {
                anyhow::bail!("{function}: bb doesn't support {rule}'s '{name}' yet");
            }
        }
        Ok(Forwarded { attrs })
    }

    fn get(&self, name: &str) -> Option<Value<'v>> {
        self.attrs.iter().find(|(key, _)| key.as_str() == name).map(|(_, value)| *value)
    }

    fn string(&self, name: &str) -> anyhow::Result<String> {
        match self.get(name) {
            None => Ok(String::new()),
            Some(value) => value.unpack_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("'{name}' must be a string")),
        }
    }

    fn strings(&self, name: &str) -> anyhow::Result<Vec<String>> {
        match self.get(name) {
            None => Ok(Vec::new()),
            Some(value) => bazel_starlark::convert::strings(value).ok_or_else(|| anyhow::anyhow!("'{name}' must be a list of strings")),
        }
    }

    fn bool(&self, name: &str) -> anyhow::Result<bool> {
        match self.get(name) {
            None => Ok(false),
            Some(value) => value.unpack_bool().ok_or_else(|| anyhow::anyhow!("'{name}' must be a bool")),
        }
    }

    /// The strip of the patches: `patch_strip`, or `patch_args` if it's
    /// just `-pN`, as Bazel applies patches itself only then.
    fn patch_strip(&self) -> anyhow::Result<i64> {
        let strip = match self.get("patch_strip") {
            None => 0,
            Some(value) => value.unpack_i32().ok_or_else(|| anyhow::anyhow!("'patch_strip' must be an int"))? as i64,
        };
        let args = self.strings("patch_args")?;
        match args.as_slice() {
            [] => Ok(strip),
            [arg] => match arg.strip_prefix("-p").and_then(|count| count.parse().ok()) {
                Some(count) => Ok(count),
                None => anyhow::bail!("bb doesn't support patch_args other than -pN yet"),
            },
            _ => anyhow::bail!("bb doesn't support patch_args other than -pN yet"),
        }
    }
}

/// Whether a `bazel_compatibility` entry is a comparison and a version of
/// three numbers, such as `>=7.0.0`.
fn valid_bazel_compatibility(text: &str) -> bool {
    let version = ["<=", ">=", "<", ">", "-"].iter().find_map(|sign| text.strip_prefix(sign));
    let Some(version) = version else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Whether Bazel `version` satisfies a `bazel_compatibility` entry, as
/// Bazel compares them: by release numbers, a prerelease below its
/// release.
pub fn satisfies_bazel_compatibility(version: &str, entry: &str) -> bool {
    let cut = if entry.contains('=') { 2 } else { 1 };
    let (sign, wanted) = entry.split_at(cut.min(entry.len()));
    let numbers = |text: &str| -> Vec<u64> { text.split('.').map_while(|part| part.parse().ok()).collect() };
    let release_end = version.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(version.len());
    let (release, suffix) = version.split_at(release_end);
    let mut ordering = numbers(release).cmp(&numbers(wanted));
    if ordering.is_eq() && suffix.starts_with("-pre") {
        ordering = std::cmp::Ordering::Less;
    }
    (ordering.is_eq() && sign.contains('='))
        || (ordering.is_gt() && (sign.contains('>') || sign.contains('-')))
        || (ordering.is_lt() && (sign.contains('<') || sign.contains('-')))
}

/// Reads files that MODULE.bazel `include()`s, by label.
pub trait IncludeReader {
    fn read(&self, label: &str) -> anyhow::Result<String>;
}

/// The state of a MODULE.bazel evaluation, reached through `Evaluator::extra`.
#[derive(ProvidesStaticType)]
struct ModuleContext<'a> {
    file: RefCell<ModuleFile>,
    is_root: bool,
    includes: Option<&'a dyn IncludeReader>,
    module_called: RefCell<bool>,
}

impl ModuleContext<'_> {
    /// Warn of the call being evaluated, if this is the root module, as
    /// Bazel only warns of the root module's file.
    fn warn(&self, eval: &Evaluator<'_, '_, '_>, message: &str) {
        let Some(span) = eval.call_stack_top_location().filter(|_| self.is_root) else {
            return;
        };
        let position = bazel_starlark::reported::call_position(&span);
        self.file.borrow_mut().warnings.push((position.line, position.column, message.to_string()));
    }

    fn get<'b>(eval: &Evaluator<'_, 'b, '_>) -> anyhow::Result<&'b ModuleContext<'b>> {
        let extra = eval.extra.ok_or_else(|| anyhow::anyhow!("not evaluating a MODULE.bazel file"))?;
        extra
            .downcast_ref::<ModuleContext>()
            .ok_or_else(|| anyhow::anyhow!("not evaluating a MODULE.bazel file"))
    }

    /// Whether a declaration should be dropped: dev dependencies only count
    /// in the root module.
    fn ignores(&self, dev: bool) -> bool {
        dev && !self.is_root
    }
}

/// Evaluate a MODULE.bazel file. Only the root module's dev dependencies
/// and overrides are kept. `includes` reads the files the module includes,
/// and is only given for the root module and modules with non-registry
/// overrides, which are the ones that may include files.
pub fn evaluate(file_name: &str, source: &str, is_root: bool, includes: Option<&dyn IncludeReader>) -> anyhow::Result<ModuleFile> {
    let context = ModuleContext {
        file: RefCell::new(ModuleFile::default()),
        is_root,
        includes,
        module_called: RefCell::new(false),
    };
    evaluate_into(file_name, source, &context)?;
    let mut file = context.file.into_inner();
    if !is_root {
        file.overrides.clear();
    }
    Ok(file)
}

fn evaluate_into(file_name: &str, source: &str, context: &ModuleContext<'_>) -> anyhow::Result<()> {
    evaluate_reporting(file_name, source, context).map_err(|error| anyhow::anyhow!("{error}"))
}

fn evaluate_reporting(file_name: &str, source: &str, context: &ModuleContext<'_>) -> Result<(), starlark::Error> {
    let ast = AstModule::parse(file_name, source.to_string(), &bazel_starlark::dialect())?;
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(context);
        eval.eval_module(ast, &MODULE_GLOBALS)?;
        Ok(())
    })
}

/// A root MODULE.bazel file that couldn't be evaluated: the error, as
/// Bazel reports it, and Bazel's summary of what failed.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ModuleFileError {
    pub error: PackageError,
    pub summary: String,
}

/// Evaluate the root module's MODULE.bazel, reporting an error as Bazel
/// does.
pub fn evaluate_root(source: &str, includes: Option<&dyn IncludeReader>) -> Result<ModuleFile, ModuleFileError> {
    let context = ModuleContext {
        file: RefCell::new(ModuleFile::default()),
        is_root: true,
        includes,
        module_called: RefCell::new(false),
    };
    if let Err(error) = evaluate_reporting("MODULE.bazel", source, &context) {
        let what = match error.kind() {
            starlark::ErrorKind::Parser(_) => "error parsing",
            starlark::ErrorKind::Scope(_) => "syntax error in",
            _ => "error executing",
        };
        return Err(ModuleFileError {
            error: bazel_starlark::reported::package_error(&error, &[]),
            summary: format!("{what} MODULE.bazel file for <root>"),
        });
    }
    Ok(context.file.into_inner())
}

static MODULE_GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    GlobalsBuilder::extended_by(&[LibraryExtension::StructType, LibraryExtension::Json, LibraryExtension::Print])
        .with(module_functions)
        .build_named(starlark::environment::GlobalFrozenHeapName { name: "bb module" })
});

/// The usage index of a proxy for a dev-only use in a non-root module,
/// whose tags and imports are all dropped.
const IGNORED_USAGE: usize = usize::MAX;

/// What `use_extension` returns: tags are created by calling its
/// attributes, as in `rust.toolchain(...)`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
struct ExtensionProxy {
    usage: usize,
    dev: bool,
}

starlark_simple_value!(ExtensionProxy);

impl fmt::Display for ExtensionProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module extension proxy>")
    }
}

#[starlark_value(type = "module_extension_proxy")]
impl<'v> StarlarkValue<'v> for ExtensionProxy {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(TagMaker {
            usage: self.usage,
            dev: self.dev,
            class: attribute.to_string(),
        }))
    }
}

/// `proxy.tag_class`, which adds a tag when called.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
struct TagMaker {
    usage: usize,
    dev: bool,
    class: String,
}

starlark_simple_value!(TagMaker);

impl fmt::Display for TagMaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag class {}>", self.class)
    }
}

#[starlark_value(type = "tag_class_proxy")]
impl<'v> StarlarkValue<'v> for TagMaker {
    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let context = ModuleContext::get(eval).map_err(starlark::Error::new_other)?;
        if context.ignores(self.dev) {
            return Ok(Value::new_none());
        }
        let mut attrs = Vec::with_capacity(names.len());
        for (name, value) in names.iter() {
            attrs.push((name.as_str().to_string(), StarValue::from_value(*value).map_err(starlark::Error::new_other)?));
        }
        let location = eval.call_stack_top_location().map(|span| span.to_string()).unwrap_or_default();
        let tag = Tag {
            class: self.class.clone(),
            attrs,
            dev: self.dev,
            location,
        };
        context.file.borrow_mut().extension_usages[self.usage].tags.push(tag);
        Ok(Value::new_none())
    }
}

/// What `use_repo_rule` returns: calling it defines a repository.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
struct RepoRuleProxy {
    bzl: String,
    rule: String,
    dev: bool,
}

starlark_simple_value!(RepoRuleProxy);

impl fmt::Display for RepoRuleProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repo rule proxy {}>", self.rule)
    }
}

#[starlark_value(type = "repo_rule_proxy")]
impl<'v> StarlarkValue<'v> for RepoRuleProxy {
    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let context = ModuleContext::get(eval).map_err(starlark::Error::new_other)?;
        if context.ignores(self.dev) {
            return Ok(Value::new_none());
        }
        let mut name = None;
        let mut attrs = Vec::with_capacity(names.len());
        for (attr, value) in names.iter() {
            if attr.as_str() == "name" {
                name = value.unpack_str().map(str::to_string);
                continue;
            }
            attrs.push((attr.as_str().to_string(), StarValue::from_value(*value).map_err(starlark::Error::new_other)?));
        }
        let Some(name) = name else {
            return Err(starlark::Error::new_other(anyhow::anyhow!("{}(): missing 'name'", self.rule)));
        };
        context.file.borrow_mut().repo_rules.push(RepoRuleCall {
            bzl: self.bzl.clone(),
            rule: self.rule.clone(),
            name,
            attrs,
            dev: self.dev,
        });
        Ok(Value::new_none())
    }
}

fn strings(value: Value<'_>) -> Vec<String> {
    bazel_starlark::convert::strings(value).unwrap_or_default()
}

#[starlark_module]
fn module_functions(builder: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named, default = "")] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = -1)] compatibility_level: i32,
        #[starlark(require = named, default = "")] repo_name: &str,
        #[starlark(require = named)] bazel_compatibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = ModuleContext::get(eval)?;
        if context.module_called.replace(true) {
            anyhow::bail!("the module() directive can only be called once");
        }
        if compatibility_level != -1 {
            context.warn(eval, "The attribute 'compatibility_level' in module() is a no-op and will be removed in a future Bazel release. Please remove it from your MODULE.bazel file.");
        }
        let earlier = ModuleFile {
            warnings: context.file.borrow().warnings.clone(),
            ..ModuleFile::default()
        };
        if *context.file.borrow() != earlier {
            anyhow::bail!("if module() is called, it must be called before any other functions");
        }
        let mut file = context.file.borrow_mut();
        file.name = name.to_string();
        file.version = version.to_string();
        file.repo_name = (!repo_name.is_empty()).then(|| repo_name.to_string());
        file.bazel_compatibility = bazel_compatibility.map(strings).unwrap_or_default();
        if let Some(invalid) = file.bazel_compatibility.iter().find(|version| !valid_bazel_compatibility(version)) {
            anyhow::bail!(
                "invalid version argument '{invalid}': valid argument must 1) start with (<,<=,>,>=,-); 2) contain a version number in form of X.X.X where X is a number"
            );
        }
        Ok(NoneType)
    }

    fn bazel_dep<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = -1)] max_compatibility_level: i32,
        #[starlark(require = named)] repo_name: Option<Value<'v>>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = ModuleContext::get(eval)?;
        if context.ignores(dev_dependency) {
            return Ok(NoneType);
        }
        if max_compatibility_level != -1 {
            context.warn(eval, "The attribute 'max_compatibility_level' in bazel_dep() is a no-op and will be removed in a future Bazel release. Please remove it from your MODULE.bazel file.");
        }
        let repo_name = match repo_name {
            None => Some(name.to_string()),
            Some(value) if value.is_none() => None,
            Some(value) => Some(value.unpack_str().unwrap_or(name).to_string()),
        };
        context.file.borrow_mut().deps.push(BazelDep {
            name: name.to_string(),
            version: version.to_string(),
            max_compatibility_level: -1,
            repo_name,
            dev: dev_dependency,
        });
        Ok(NoneType)
    }

    fn use_extension<'v>(
        #[starlark(require = pos)] extension_bzl_file: &str,
        #[starlark(require = pos)] extension_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        #[starlark(require = named, default = false)] isolate: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<ExtensionProxy> {
        let context = ModuleContext::get(eval)?;
        if context.ignores(dev_dependency) {
            return Ok(ExtensionProxy {
                usage: IGNORED_USAGE,
                dev: true,
            });
        }
        let mut file = context.file.borrow_mut();
        let existing = file
            .extension_usages
            .iter()
            .position(|usage| !isolate && !usage.isolate && usage.bzl == extension_bzl_file && usage.name == extension_name);
        let usage = match existing {
            Some(index) => {
                file.extension_usages[index].dev_only &= dev_dependency;
                index
            }
            None => {
                file.extension_usages.push(ExtensionUsage {
                    bzl: extension_bzl_file.to_string(),
                    name: extension_name.to_string(),
                    isolate,
                    imports: Vec::new(),
                    tags: Vec::new(),
                    repo_overrides: Vec::new(),
                    dev_only: dev_dependency,
                });
                file.extension_usages.len() - 1
            }
        };
        Ok(ExtensionProxy { usage, dev: dev_dependency })
    }

    fn use_repo<'v>(
        #[starlark(require = pos)] extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<&str>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let Some(proxy) = extension_proxy.downcast_ref::<ExtensionProxy>() else {
            anyhow::bail!("use_repo() takes the result of use_extension()");
        };
        let context = ModuleContext::get(eval)?;
        if context.ignores(proxy.dev) {
            return Ok(NoneType);
        }
        let mut file = context.file.borrow_mut();
        let usage = &mut file.extension_usages[proxy.usage];
        for name in args.items {
            usage.imports.push(RepoImport {
                apparent: name.to_string(),
                exported: name.to_string(),
                dev: proxy.dev,
            });
        }
        for (apparent, exported) in kwargs {
            let Some(exported) = exported.unpack_str() else {
                anyhow::bail!("use_repo(): repository names must be strings");
            };
            usage.imports.push(RepoImport {
                apparent: apparent.as_str().to_string(),
                exported: exported.to_string(),
                dev: proxy.dev,
            });
        }
        Ok(NoneType)
    }

    fn override_repo<'v>(
        #[starlark(require = pos)] extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<&str>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        repo_overrides(extension_proxy, args, kwargs, eval)
    }

    fn inject_repo<'v>(
        #[starlark(require = pos)] extension_proxy: Value<'v>,
        #[starlark(args)] args: UnpackTuple<&str>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        repo_overrides(extension_proxy, args, kwargs, eval)
    }

    fn use_repo_rule<'v>(
        #[starlark(require = pos)] repo_rule_bzl_file: &str,
        #[starlark(require = pos)] repo_rule_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
    ) -> anyhow::Result<RepoRuleProxy> {
        Ok(RepoRuleProxy {
            bzl: repo_rule_bzl_file.to_string(),
            rule: repo_rule_name.to_string(),
            dev: dev_dependency,
        })
    }

    fn register_toolchains<'v>(
        #[starlark(args)] args: UnpackTuple<&str>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = ModuleContext::get(eval)?;
        if !context.ignores(dev_dependency) {
            context.file.borrow_mut().toolchains.extend(args.items.iter().map(|s| s.to_string()));
        }
        Ok(NoneType)
    }

    fn register_execution_platforms<'v>(
        #[starlark(args)] args: UnpackTuple<&str>,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = ModuleContext::get(eval)?;
        if !context.ignores(dev_dependency) {
            context.file.borrow_mut().execution_platforms.extend(args.items.iter().map(|s| s.to_string()));
        }
        Ok(NoneType)
    }

    fn include<'v>(
        #[starlark(require = pos)] label: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let context = ModuleContext::get(eval)?;
        if context.ignores(dev_dependency) {
            return Ok(NoneType);
        }
        let Some(includes) = context.includes else {
            anyhow::bail!("include() can only be used in the root module or in modules with non-registry overrides");
        };
        let source = includes.read(label)?;
        evaluate_into(label, &source, context)?;
        Ok(NoneType)
    }

    fn single_version_override<'v>(
        #[starlark(require = named)] module_name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = "")] registry: &str,
        #[starlark(require = named)] patches: Option<Value<'v>>,
        #[starlark(require = named)] patch_cmds: Option<Value<'v>>,
        #[starlark(require = named, default = 0)] patch_strip: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let override_ = Override::SingleVersion {
            version: version.to_string(),
            registry: registry.to_string(),
            patches: patches.map(strings).unwrap_or_default(),
            patch_cmds: patch_cmds.map(strings).unwrap_or_default(),
            patch_strip: patch_strip as i64,
        };
        add_override(eval, module_name, override_)
    }

    fn multiple_version_override<'v>(
        #[starlark(require = named)] module_name: &str,
        #[starlark(require = named)] versions: Value<'v>,
        #[starlark(require = named, default = "")] registry: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let versions = strings(versions);
        for version in &versions {
            if crate::version::Version::parse(version).is_err() {
                anyhow::bail!("Invalid version in multiple_version_override()");
            }
        }
        if versions.len() < 2 {
            anyhow::bail!("multiple_version_override() must specify at least 2 versions");
        }
        let override_ = Override::MultipleVersion {
            versions,
            registry: registry.to_string(),
        };
        add_override(eval, module_name, override_)
    }

    /// Fetch a module from an archive, with `http_archive`'s attributes.
    fn archive_override<'v>(
        #[starlark(require = named)] module_name: &str,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let attrs = Forwarded::check(kwargs, "archive_override", "http_archive", HTTP_ARCHIVE_ATTRS, ARCHIVE_OVERRIDE_ATTRS)?;
        let mut urls = attrs.strings("urls")?;
        let url = attrs.string("url")?;
        if !url.is_empty() {
            urls.insert(0, url);
        }
        let integrity = match attrs.string("integrity")? {
            integrity if integrity.is_empty() => attrs.string("sha256")?,
            integrity => integrity,
        };
        let override_ = Override::Archive {
            urls,
            integrity,
            strip_prefix: attrs.string("strip_prefix")?,
            archive_type: attrs.string("type")?,
            patches: attrs.strings("patches")?,
            patch_cmds: attrs.strings("patch_cmds")?,
            patch_strip: attrs.patch_strip()?,
        };
        add_override(eval, module_name, override_)
    }

    /// Fetch a module from a Git repository, with `git_repository`'s
    /// attributes.
    fn git_override<'v>(
        #[starlark(require = named)] module_name: &str,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let attrs = Forwarded::check(kwargs, "git_override", "git_repository", GIT_REPOSITORY_ATTRS, GIT_OVERRIDE_ATTRS)?;
        let source = GitSource {
            remote: attrs.string("remote")?,
            commit: attrs.string("commit")?,
            tag: attrs.string("tag")?,
            branch: attrs.string("branch")?,
            shallow_since: attrs.string("shallow_since")?,
            init_submodules: attrs.bool("init_submodules")?,
            recursive_init_submodules: attrs.bool("recursive_init_submodules")?,
            strip_prefix: attrs.string("strip_prefix")?,
            patches: attrs.strings("patches")?,
            patch_cmds: attrs.strings("patch_cmds")?,
            patch_strip: attrs.patch_strip()?,
        };
        if source.remote.is_empty() {
            anyhow::bail!("git_override: missing value for mandatory attribute 'remote' in 'git_repository' rule");
        }
        add_override(eval, module_name, Override::Git(source))
    }

    fn local_path_override<'v>(
        #[starlark(require = named)] module_name: &str,
        #[starlark(require = named)] path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        add_override(eval, module_name, Override::LocalPath { path: path.to_string() })
    }

    fn flag_alias<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<NoneType> {
        let _ = kwargs;
        Ok(NoneType)
    }
}

fn add_override(eval: &mut Evaluator<'_, '_, '_>, module_name: &str, override_: Override) -> anyhow::Result<NoneType> {
    let context = ModuleContext::get(eval)?;
    let mut file = context.file.borrow_mut();
    if file.overrides.iter().any(|(name, _)| name == module_name) {
        anyhow::bail!("multiple overrides for dep {module_name} found");
    }
    file.overrides.push((module_name.to_string(), override_));
    Ok(NoneType)
}

fn repo_overrides<'v>(
    extension_proxy: Value<'v>,
    args: UnpackTuple<&str>,
    kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<NoneType> {
    let Some(proxy) = extension_proxy.downcast_ref::<ExtensionProxy>() else {
        anyhow::bail!("expected the result of use_extension()");
    };
    let context = ModuleContext::get(eval)?;
    if context.ignores(proxy.dev) {
        return Ok(NoneType);
    }
    let mut file = context.file.borrow_mut();
    let usage = &mut file.extension_usages[proxy.usage];
    for name in args.items {
        usage.repo_overrides.push((name.to_string(), name.to_string()));
    }
    for (exported, apparent) in kwargs {
        if let Some(apparent) = apparent.unpack_str() {
            usage.repo_overrides.push((exported.as_str().to_string(), apparent.to_string()));
        }
    }
    Ok(NoneType)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Includes;

    impl IncludeReader for Includes {
        fn read(&self, label: &str) -> anyhow::Result<String> {
            assert_eq!(label, "//third_party:deps.MODULE.bazel");
            Ok("bazel_dep(name = 'included', version = '1.0')\n".to_string())
        }
    }

    const MODULE: &str = r#"
module(name = "app", version = "0.1", repo_name = "my_app")

bazel_dep(name = "rules_rust", version = "0.74.0")
bazel_dep(name = "nodep", version = "2.0", repo_name = None)
bazel_dep(name = "devonly", version = "1.0", dev_dependency = True)

rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")
rust.toolchain(edition = "2021", versions = ["1.98.0"])
use_repo(rust, "rust_toolchains", alias = "rust_host_tools")

http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(name = "sysroot", urls = ["https://x"], sha256 = "abc")

register_toolchains("@rust_toolchains//:all")
single_version_override(module_name = "rules_rust", version = "0.75.0")
include("//third_party:deps.MODULE.bazel")
"#;

    #[test]
    fn root_modules_keep_everything() {
        let file = evaluate("MODULE.bazel", MODULE, true, Some(&Includes)).unwrap();
        assert_eq!(file.name, "app");
        assert_eq!(file.repo_name.as_deref(), Some("my_app"));
        let deps: Vec<(&str, Option<&str>)> = file.deps.iter().map(|d| (d.name.as_str(), d.repo_name.as_deref())).collect();
        assert_eq!(
            deps,
            [("rules_rust", Some("rules_rust")), ("nodep", None), ("devonly", Some("devonly")), ("included", Some("included"))]
        );
        let usage = &file.extension_usages[0];
        assert_eq!(usage.tags[0].class, "toolchain");
        assert_eq!(usage.tags[0].attrs[0], ("edition".to_string(), StarValue::String("2021".into())));
        assert_eq!(usage.imports[1].apparent, "alias");
        assert_eq!(usage.imports[1].exported, "rust_host_tools");
        assert_eq!(file.repo_rules[0].name, "sysroot");
        assert_eq!(file.toolchains, ["@rust_toolchains//:all"]);
        assert_eq!(file.overrides.len(), 1);
    }

    #[test]
    fn other_modules_drop_dev_dependencies_and_overrides() {
        let module = MODULE.replace("include(\"//third_party:deps.MODULE.bazel\")", "");
        let file = evaluate("MODULE.bazel", &module, false, None).unwrap();
        assert!(file.deps.iter().all(|dep| dep.name != "devonly"));
        assert!(file.overrides.is_empty());
    }

    #[test]
    fn overridden_modules_include_files() {
        let file = evaluate("MODULE.bazel", MODULE, false, Some(&Includes)).unwrap();
        assert!(file.deps.iter().any(|dep| dep.name == "included"));
    }

    #[test]
    fn registry_modules_cant_include_files() {
        let error = evaluate("MODULE.bazel", MODULE, false, None).unwrap_err();
        assert!(error.to_string().contains("modules with non-registry overrides"), "{error}");
    }

    #[test]
    fn other_modules_skip_dev_includes() {
        let module = MODULE.replace(".MODULE.bazel\")", ".MODULE.bazel\", dev_dependency = True)");
        let file = evaluate("MODULE.bazel", &module, false, None).unwrap();
        assert!(file.deps.iter().all(|dep| dep.name != "included"));
    }

    #[test]
    fn bazel_compatibility_compares_as_bazel_does() {
        assert!(satisfies_bazel_compatibility("9.3.1", ">=7.0.0"));
        assert!(satisfies_bazel_compatibility("9.3.1", "<=9.3.1"));
        assert!(!satisfies_bazel_compatibility("9.3.1", "<9.0.0"));
        assert!(!satisfies_bazel_compatibility("9.3.1", "-9.3.1"));
        assert!(satisfies_bazel_compatibility("9.3.1", "-8.0.0"));
        assert!(!satisfies_bazel_compatibility("9.3.1-pre.1", ">=9.3.1"));
        assert!(valid_bazel_compatibility(">=7.0.0") && !valid_bazel_compatibility("7.0.0") && !valid_bazel_compatibility(">=7.0"));
    }
}
