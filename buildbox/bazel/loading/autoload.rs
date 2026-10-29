//! Bazel's `--incompatible_autoload_externally`: symbols that were once
//! built into Bazel, given to BUILD and `.bzl` files from the rule sets
//! that define them now, as if those files loaded them.
//!
//! Each command sets the [`AutoloadSettings`] of its workspace. BUILD
//! files, and `.bzl` files they load outside the rule sets' own
//! repositories, are evaluated with the globals [`AutoloadGlobalsKey`]
//! makes from them.

use crate::bzl::{BzlContext, BzlModule};
use bazel_label::{Label, RepoName};
use bazel_starlark::AutoloadChanges;
use bazel_starlark::autoload::AutoloadUnavailable;
use skycodec::{Decode, Encode, Fingerprint, Fingerprinter};
use skyframe::{Ctx, Key, SkyError, Storage, Value, encoded_value};
use starlark::environment::Globals;
use starlark::values::{FrozenHeap, FrozenValue};
use std::sync::{Arc, Mutex};

/// The options that say which symbols are autoloaded, and where.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AutoloadSettings {
    /// `--incompatible_autoload_externally`: symbols, or `@repo` for all
    /// of a rule set's, each optionally prefixed with `+` or `-`.
    pub symbols: Vec<String>,
    /// `--incompatible_disable_autoloads_in_main_repo`.
    pub disable_in_main_repo: bool,
    /// `--repositories_without_autoloads`.
    pub repositories_without: Vec<String>,
}

impl Default for AutoloadSettings {
    /// Bazel 9's defaults: no autoloads.
    fn default() -> AutoloadSettings {
        AutoloadSettings {
            symbols: Vec::new(),
            disable_in_main_repo: true,
            repositories_without: Vec::new(),
        }
    }
}

/// A graph service: the autoload settings of the latest command.
#[derive(Default)]
pub struct CommandAutoloads(Mutex<AutoloadSettings>);

impl CommandAutoloads {
    pub fn new() -> Arc<CommandAutoloads> {
        Arc::default()
    }

    /// Set the settings, returning whether they changed, in which case
    /// [`AutoloadSettingsKey`] must be invalidated.
    pub fn set(&self, settings: AutoloadSettings) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != settings;
        *current = settings;
        changed
    }
}

/// The autoload settings, checked and with `@repo` expanded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Autoloads {
    /// Symbols loaded from the rule sets, where autoloads apply.
    pub autoloaded: Vec<String>,
    /// Symbols removed everywhere: those prefixed with `-`.
    pub removed: Vec<String>,
    /// Symbols removed where autoloads don't apply: those not prefixed
    /// with `+`.
    pub partially_removed: Vec<String>,
    /// Repositories, by the name before their canonical name's `+`, whose
    /// `.bzl` files don't get autoloads.
    pub repositories_without: Vec<String>,
    pub disable_in_main_repo: bool,
}

encoded_value!(Autoloads);

impl Autoloads {
    pub fn enabled(&self) -> bool {
        !self.autoloaded.is_empty() || !self.removed.is_empty() || !self.partially_removed.is_empty()
    }

    /// Whether `.bzl` files in `repo` get autoloads.
    pub fn in_bzl(&self, repo: RepoName) -> bool {
        if !self.enabled() || (self.disable_in_main_repo && repo.is_main()) {
            return false;
        }
        let name = repo.as_str();
        let base = name.split(['+', '~']).next().unwrap_or(name);
        !self.repositories_without.iter().any(|without| without == base)
    }

    /// Whether BUILD files in `repo` get autoloads.
    pub fn in_build(&self, repo: RepoName) -> bool {
        self.enabled() && !(self.disable_in_main_repo && repo.is_main())
    }

    /// The context to load a `.bzl` file in `repo` in, for a BUILD file.
    pub fn bzl_context(&self, repo: RepoName) -> BzlContext {
        match (self.enabled(), self.in_bzl(repo)) {
            (false, _) => BzlContext::Plain,
            (true, false) => BzlContext::Build,
            (true, true) => BzlContext::Autoloads,
        }
    }
}

/// Checks and expands the settings of the latest command.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct AutoloadSettingsKey;

impl Key for AutoloadSettingsKey {
    type Value = Autoloads;
    const KIND: &'static str = "AUTOLOAD_SETTINGS";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<Autoloads, SkyError> {
        let settings = match ctx.global::<CommandAutoloads>() {
            Ok(command) => command.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone(),
            Err(_) => AutoloadSettings::default(),
        };
        expand(&settings).map_err(SkyError::new)
    }
}

/// Check the settings and expand `@repo` into the symbols that rule set
/// defines, as Bazel does.
pub fn expand(settings: &AutoloadSettings) -> Result<Autoloads, String> {
    let mut symbols = Vec::new();
    for given in settings.symbols.iter().filter(|symbol| !symbol.is_empty()) {
        let (prefix, symbol) = split_prefix(given);
        match symbol.strip_prefix('@') {
            Some(repo) => symbols.extend(redirects_in(repo).map(|redirect| format!("{prefix}{}", redirect.symbol))),
            None => symbols.push(given.clone()),
        }
    }
    let mut seen = Vec::new();
    for symbol in &symbols {
        let (_, name) = split_prefix(symbol);
        if seen.contains(&name) {
            return Err(format!("Duplicated symbol '{name}' in --incompatible_autoload_externally"));
        }
        if redirect(name).is_none() {
            return Err(format!("Undefined symbol in --incompatible_autoload_externally: {name}"));
        }
        seen.push(name);
    }
    let is_proto_common = |symbol: &str| symbol.ends_with("proto_common_do_not_use");
    let mut repositories_without: Vec<String> = REPOS_WITHOUT_AUTOLOADS.iter().map(|repo| repo.to_string()).collect();
    repositories_without.extend(settings.repositories_without.iter().filter(|repo| !repo.is_empty()).cloned());
    Ok(Autoloads {
        autoloaded: unprefixed(&symbols, |symbol| !symbol.starts_with('-') && !is_proto_common(symbol)),
        removed: unprefixed(&symbols, |symbol| symbol.starts_with('-') || is_proto_common(symbol)),
        partially_removed: unprefixed(&symbols, |symbol| !symbol.starts_with('+')),
        repositories_without,
        disable_in_main_repo: settings.disable_in_main_repo,
    })
}

fn split_prefix(symbol: &str) -> (&str, &str) {
    match symbol.chars().next() {
        Some('+' | '-') => symbol.split_at(1),
        _ => ("", symbol),
    }
}

/// The symbols `keep` keeps, without their prefixes.
fn unprefixed(symbols: &[String], keep: impl Fn(&str) -> bool) -> Vec<String> {
    symbols.iter().filter(|symbol| keep(symbol)).map(|symbol| split_prefix(symbol).1.to_string()).collect()
}

/// The globals of files, as the autoload settings change those builtins
/// give them: `with` autoloads, or without, for files where autoloads
/// don't apply.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct AutoloadGlobalsKey {
    pub with: bool,
}

/// Globals for `.bzl` and BUILD files.
pub struct AutoloadGlobals {
    pub bzl: Globals,
    pub build: Globals,
    fingerprint: Fingerprint,
    /// The modules the autoloaded values are in, kept alive while the
    /// globals are.
    _modules: Vec<Arc<BzlModule>>,
}

impl Value for AutoloadGlobals {
    fn to_storage(&self) -> Storage {
        Storage::Live {
            fingerprint: self.fingerprint,
            size: 0,
        }
    }

    fn from_bytes(_bytes: &[u8]) -> Result<AutoloadGlobals, skycodec::DecodeError> {
        Err(skycodec::DecodeError::Invalid {
            what: "live autoload globals",
            value: 0,
            position: 0,
        })
    }
}

impl Key for AutoloadGlobalsKey {
    type Value = AutoloadGlobals;
    const KIND: &'static str = "AUTOLOAD_GLOBALS";

    async fn compute(&self, ctx: &Ctx) -> Result<AutoloadGlobals, SkyError> {
        let builtins = ctx.compute(&crate::BuiltinsKey).await?;
        let autoloads = ctx.compute(&AutoloadSettingsKey).await?;
        let Some(starlark_builtins) = builtins.starlark_builtins() else {
            return Ok(AutoloadGlobals {
                bzl: builtins.bzl.clone(),
                build: builtins.build.clone(),
                fingerprint: Fingerprint::of(b"no builtins"),
                _modules: Vec::new(),
            });
        };
        check_removable(&autoloads, &builtins)?;
        let loaded = match self.with {
            true => load_autoloaded(ctx, &autoloads).await?,
            false => Vec::new(),
        };
        let fingerprint = globals_fingerprint(self.with, &autoloads, &loaded);
        let heap = FrozenHeap::new();
        let mut changes = AutoloadChanges {
            removed: removed_names(self.with, &autoloads),
            id: fingerprint.to_string(),
            ..AutoloadChanges::default()
        };
        let mut modules = Vec::new();
        for (redirect, module) in &loaded {
            let value = match module {
                Some(module) => exported_value(redirect, module)?,
                None => heap.alloc_simple(unavailable(redirect)),
            };
            match redirect.rule {
                true => changes.rules.push((redirect.symbol.to_string(), value)),
                false => changes.toplevels.push((redirect.symbol.to_string(), value)),
            }
            modules.extend(module.clone());
        }
        let heap = heap.into_ref_named(starlark::values::FrozenHeapName::User(Box::new(format!("autoload placeholders {fingerprint}"))));
        changes.heaps = modules.iter().map(|module| module.module.frozen_heap()).collect();
        changes.heaps.push(&heap);
        Ok(AutoloadGlobals {
            bzl: bazel_starlark::bzl_globals_changed(&starlark_builtins, &changes),
            build: bazel_starlark::build_globals_changed(&starlark_builtins, &changes),
            fingerprint,
            _modules: modules,
        })
    }
}

/// The names removed from globals `with` autoloads or without. Bazel keeps
/// `proto_common_do_not_use` with only the flag protobuf's rules read, and
/// bb's stand-in for it only answers flags anyway, so it stays.
fn removed_names(with: bool, autoloads: &Autoloads) -> Vec<String> {
    let removed = match with {
        true => &autoloads.removed,
        false => &autoloads.partially_removed,
    };
    removed.iter().filter(|symbol| *symbol != "proto_common_do_not_use").cloned().collect()
}

/// Fail if a symbol is removed that a builtin still uses, as Bazel does.
fn check_removable(autoloads: &Autoloads, builtins: &crate::Builtins) -> Result<(), SkyError> {
    if autoloads.partially_removed.iter().any(|symbol| symbol == "ProtoInfo") && !autoloads.removed.iter().any(|symbol| symbol == "proto_common_do_not_use") {
        return Err(SkyError::new("Symbol in 'ProtoInfo' can't be removed, because it's still used by: 'proto_common_do_not_use'"));
    }
    let available = |name: &str| builtins.build.iter().any(|(global, _)| global == name) || builtins.bzl.iter().any(|(global, _)| global == name);
    for symbol in &autoloads.partially_removed {
        let Some(redirect) = redirect(symbol) else {
            continue;
        };
        let used_by: Vec<&str> = redirect.rdeps.iter().copied().filter(|rdep| available(rdep)).collect();
        if !used_by.is_empty() {
            return Err(SkyError::new(format!("Symbol '{symbol}' can't be removed, because it's still used by: {}", used_by.join(", "))));
        }
    }
    Ok(())
}

/// Load each autoloaded symbol's `.bzl` file, or `None` for those whose
/// module the build doesn't depend on.
async fn load_autoloaded(ctx: &Ctx, autoloads: &Autoloads) -> Result<Vec<(&'static Redirect, Option<Arc<BzlModule>>)>, SkyError> {
    let repositories = ctx.global::<crate::Repositories>()?;
    let mut keys = Vec::new();
    let mut redirects = Vec::new();
    for symbol in &autoloads.autoloaded {
        let Some(redirect) = redirect(symbol) else {
            continue;
        };
        let newer_than = MINIMUM_VERSIONS.iter().find(|(module, _)| *module == redirect.module()).map(|(_, version)| *version);
        let repo = repositories.0.module_repo(ctx, redirect.module(), newer_than).await?;
        let label = match repo {
            Some(repo) => Some(redirect.label_in(repo)?),
            None => None,
        };
        redirects.push((redirect, label));
    }
    for (_, label) in &redirects {
        if let Some(label) = label {
            keys.push(crate::bzl::bzl_key_for_build(ctx, *label).await?);
        }
    }
    let mut modules = ctx.compute_many(&keys).await.into_iter();
    let mut loaded = Vec::with_capacity(redirects.len());
    for (redirect, label) in redirects {
        let module = match label {
            Some(_) => Some(modules.next().ok_or_else(|| SkyError::new("autoloaded module missing"))??),
            None => None,
        };
        loaded.push((redirect, module));
    }
    Ok(loaded)
}

/// The value a module exports for an autoloaded symbol, under its new
/// name if it was renamed.
fn exported_value(redirect: &Redirect, module: &BzlModule) -> Result<FrozenValue, SkyError> {
    let name = redirect.new_name.unwrap_or(redirect.symbol);
    match module.module.get_any_visibility(name) {
        Ok((value, _)) => Ok(value.value().unpack_frozen().ok_or_else(|| SkyError::new(format!("'{name}' isn't frozen")))?),
        Err(_) => Err(SkyError::new(format!(
            "The toplevel symbol '{}' set by --incompatible_load_symbols_externally couldn't be loaded. '{name}' not found in auto loaded '{}'.",
            redirect.symbol, redirect.label
        ))),
    }
}

fn unavailable(redirect: &Redirect) -> AutoloadUnavailable {
    AutoloadUnavailable {
        symbol: redirect.symbol.to_string(),
        label: redirect.label.to_string(),
        module: redirect.module().to_string(),
    }
}

fn globals_fingerprint(with: bool, autoloads: &Autoloads, loaded: &[(&Redirect, Option<Arc<BzlModule>>)]) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(&[u8::from(with)]);
    fingerprinter.bytes(&skycodec::to_bytes(autoloads));
    for (redirect, module) in loaded {
        fingerprinter.bytes(redirect.symbol.as_bytes());
        if let Some(module) = module {
            fingerprinter.fingerprint(&module.fingerprint);
        }
    }
    fingerprinter.finish()
}

/// Where a symbol that was built into Bazel is defined now.
pub struct Redirect {
    pub symbol: &'static str,
    /// The `.bzl` file defining it, in the repository its module is seen
    /// as.
    pub label: &'static str,
    /// Whether it's a rule, which BUILD files and `native` get, rather
    /// than a top-level value of `.bzl` files.
    pub rule: bool,
    /// The name the file defines it as, if not its own.
    pub new_name: Option<&'static str>,
    /// The builtins that use it, which keep it from being removed.
    pub rdeps: &'static [&'static str],
}

impl Redirect {
    /// The repository name the label uses.
    fn repo_name(&self) -> &'static str {
        self.label.trim_start_matches('@').split("//").next().unwrap_or_default()
    }

    /// The module whose repository the file is in.
    pub fn module(&self) -> &'static str {
        match self.repo_name() {
            "com_google_protobuf" => "protobuf",
            "build_bazel_apple_support" => "apple_support",
            name => name,
        }
    }

    /// The file's label, with the module's repository being `repo`.
    fn label_in(&self, repo: RepoName) -> Result<Label, SkyError> {
        let path = self.label.split_once("//").map(|(_, path)| path).unwrap_or_default();
        Label::parse_canonical(&format!("@@{repo}//{path}")).map_err(|error| SkyError::new(format!("autoload of '{}': {error}", self.symbol)))
    }
}

fn redirect(symbol: &str) -> Option<&'static Redirect> {
    REDIRECTS.iter().find(|redirect| redirect.symbol == symbol)
}

/// The symbols a rule set defines, given as `@repo`.
fn redirects_in(repo: &str) -> impl Iterator<Item = &'static Redirect> + '_ {
    REDIRECTS.iter().filter(move |redirect| redirect.label.starts_with(&format!("@{repo}//")))
}

/// Repositories whose `.bzl` files never get autoloads: the rule sets
/// themselves, and those they depend on.
const REPOS_WITHOUT_AUTOLOADS: &[&str] = &[
    "protobuf",
    "com_google_protobuf",
    "proto_bazel_features",
    "rules_android",
    "rules_cc",
    "rules_java",
    "rules_java_builtin",
    "compatibility_proxy",
    "rules_python",
    "rules_python_internal",
    "rules_shell",
    "apple_support",
    "build_bazel_apple_support",
    "bazel_skylib",
    "bazel_tools",
    "bazel_features",
    "bazel_features_version",
    "bazel_features_globals",
];

/// Modules whose versions up to these don't define their symbols yet, so
/// they aren't autoloaded from them.
const MINIMUM_VERSIONS: &[(&str, &str)] = &[("protobuf", "29.0-rc1"), ("rules_android", "0.6.0-rc1")];

const fn rule(symbol: &'static str, label: &'static str) -> Redirect {
    Redirect {
        symbol,
        label,
        rule: true,
        new_name: None,
        rdeps: &[],
    }
}

const fn value(symbol: &'static str, label: &'static str, rdeps: &'static [&'static str]) -> Redirect {
    Redirect {
        symbol,
        label,
        rule: false,
        new_name: None,
        rdeps,
    }
}

const fn renamed(symbol: &'static str, label: &'static str, new_name: &'static str, rdeps: &'static [&'static str]) -> Redirect {
    Redirect {
        symbol,
        label,
        rule: false,
        new_name: Some(new_name),
        rdeps,
    }
}

/// Every symbol that can be autoloaded, as Bazel 9.3.1's AutoloadSymbols
/// has them.
static REDIRECTS: &[Redirect] = &[
    value("CcSharedLibraryInfo", "@rules_cc//cc/common:cc_shared_library_info.bzl", &["cc_shared_library"]),
    value("CcSharedLibraryHintInfo", "@rules_cc//cc/common:cc_shared_library_hint_info.bzl", &["cc_common"]),
    value("cc_proto_aspect", "@com_google_protobuf//bazel/private:bazel_cc_proto_library.bzl", &["cc_proto_aspect"]),
    value(
        "ProtoInfo",
        "@com_google_protobuf//bazel/common:proto_info.bzl",
        &["proto_library", "cc_proto_library", "java_lite_proto_library", "java_proto_library", "proto_lang_toolchain", "java_binary"],
    ),
    value("proto_common_do_not_use", "@com_google_protobuf//:dummy.bzl", &[]),
    value("cc_common", "@rules_cc//cc/common:cc_common.bzl", &[]),
    value(
        "CcInfo",
        "@rules_cc//cc/common:cc_info.bzl",
        &[
            "cc_binary",
            "cc_library",
            "cc_test",
            "cc_shared_library",
            "cc_common",
            "java_library",
            "cc_proto_library",
            "java_import",
            "java_runtime",
            "java_binary",
            "objc_library",
            "java_common",
            "JavaInfo",
            "py_extension",
            "cc_import",
            "objc_import",
            "objc_library",
            "cc_toolchain",
            "PyCcLinkParamsProvider",
            "py_library",
        ],
    ),
    value("DebugPackageInfo", "@rules_cc//cc/common:debug_package_info.bzl", &["cc_binary", "cc_test"]),
    value("CcToolchainConfigInfo", "@rules_cc//cc/toolchains:cc_toolchain_config_info.bzl", &["cc_toolchain"]),
    value("java_common", "@rules_java//java/common:java_common.bzl", &[]),
    value(
        "JavaInfo",
        "@rules_java//java/common:java_info.bzl",
        &["java_binary", "java_library", "java_test", "java_proto_library", "java_lite_proto_library", "java_plugin", "java_import", "java_common"],
    ),
    value("JavaPluginInfo", "@rules_java//java/common:java_plugin_info.bzl", &["java_plugin", "java_library", "java_binary", "java_test"]),
    renamed(
        "ProguardSpecProvider",
        "@rules_java//java/common:proguard_spec_info.bzl",
        "ProguardSpecInfo",
        &["java_lite_proto_library", "java_import", "android_binary", "android_library"],
    ),
    value("PyInfo", "@rules_python//python:py_info.bzl", &["py_binary", "py_test", "py_library"]),
    value("PyRuntimeInfo", "@rules_python//python:py_runtime_info.bzl", &["py_binary", "py_test", "py_library"]),
    renamed("PyCcLinkParamsProvider", "@rules_python//python:py_cc_link_params_info.bzl", "PyCcLinkParamsInfo", &["py_binary", "py_test", "py_library"]),
    value(
        "AndroidIdeInfo",
        "@rules_android//providers:providers.bzl",
        &["aar_import", "android_binary", "android_library", "android_local_test", "android_sdk"],
    ),
    rule("aar_import", "@rules_android//rules:rules.bzl"),
    rule("android_binary", "@rules_android//rules:rules.bzl"),
    rule("android_library", "@rules_android//rules:rules.bzl"),
    rule("android_local_test", "@rules_android//rules:rules.bzl"),
    rule("android_sdk", "@rules_android//rules:rules.bzl"),
    rule("android_tools_defaults_jar", "@rules_android//rules:rules.bzl"),
    rule("cc_binary", "@rules_cc//cc:cc_binary.bzl"),
    rule("cc_import", "@rules_cc//cc:cc_import.bzl"),
    rule("cc_library", "@rules_cc//cc:cc_library.bzl"),
    rule("cc_proto_library", "@com_google_protobuf//bazel:cc_proto_library.bzl"),
    rule("cc_shared_library", "@rules_cc//cc:cc_shared_library.bzl"),
    rule("cc_test", "@rules_cc//cc:cc_test.bzl"),
    rule("cc_toolchain", "@rules_cc//cc/toolchains:cc_toolchain.bzl"),
    rule("cc_toolchain_suite", "@rules_cc//cc/toolchains:cc_toolchain_suite.bzl"),
    rule("fdo_prefetch_hints", "@rules_cc//cc/toolchains:fdo_prefetch_hints.bzl"),
    rule("fdo_profile", "@rules_cc//cc/toolchains:fdo_profile.bzl"),
    rule("java_binary", "@rules_java//java:java_binary.bzl"),
    rule("java_import", "@rules_java//java:java_import.bzl"),
    rule("java_library", "@rules_java//java:java_library.bzl"),
    rule("java_lite_proto_library", "@com_google_protobuf//bazel:java_lite_proto_library.bzl"),
    rule("java_package_configuration", "@rules_java//java/toolchains:java_package_configuration.bzl"),
    rule("java_plugin", "@rules_java//java:java_plugin.bzl"),
    rule("java_proto_library", "@com_google_protobuf//bazel:java_proto_library.bzl"),
    rule("java_runtime", "@rules_java//java/toolchains:java_runtime.bzl"),
    rule("java_test", "@rules_java//java:java_test.bzl"),
    rule("java_toolchain", "@rules_java//java/toolchains:java_toolchain.bzl"),
    rule("memprof_profile", "@rules_cc//cc/toolchains:memprof_profile.bzl"),
    rule("objc_import", "@rules_cc//cc:objc_import.bzl"),
    rule("objc_library", "@rules_cc//cc:objc_library.bzl"),
    rule("propeller_optimize", "@rules_cc//cc/toolchains:propeller_optimize.bzl"),
    rule("proto_lang_toolchain", "@com_google_protobuf//bazel/toolchains:proto_lang_toolchain.bzl"),
    rule("proto_library", "@com_google_protobuf//bazel:proto_library.bzl"),
    rule("py_binary", "@rules_python//python:py_binary.bzl"),
    rule("py_library", "@rules_python//python:py_library.bzl"),
    rule("py_runtime", "@rules_python//python:py_runtime.bzl"),
    rule("py_test", "@rules_python//python:py_test.bzl"),
    rule("sh_binary", "@rules_shell//shell:sh_binary.bzl"),
    rule("sh_library", "@rules_shell//shell:sh_library.bzl"),
    rule("sh_test", "@rules_shell//shell:sh_test.bzl"),
    rule("available_xcodes", "@build_bazel_apple_support//xcode:available_xcodes.bzl"),
    rule("xcode_config", "@build_bazel_apple_support//xcode:xcode_config.bzl"),
    rule("xcode_config_alias", "@build_bazel_apple_support//xcode:xcode_config_alias.bzl"),
    rule("xcode_version", "@build_bazel_apple_support//xcode:xcode_version.bzl"),
    value("apple_common", "@build_bazel_apple_support//lib:apple_common.bzl", &["objc_import", "objc_library"]),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(symbols: &[&str]) -> AutoloadSettings {
        AutoloadSettings {
            symbols: symbols.iter().map(|symbol| symbol.to_string()).collect(),
            ..AutoloadSettings::default()
        }
    }

    #[test]
    fn repos_expand_to_their_symbols() {
        let autoloads = expand(&settings(&["+@rules_shell", "-py_test"])).unwrap();
        assert_eq!(autoloads.autoloaded, ["sh_binary", "sh_library", "sh_test"]);
        assert_eq!(autoloads.removed, ["py_test"]);
        assert_eq!(autoloads.partially_removed, ["py_test"]);
    }

    #[test]
    fn unknown_and_duplicate_symbols_are_errors() {
        assert_eq!(expand(&settings(&["nope"])).unwrap_err(), "Undefined symbol in --incompatible_autoload_externally: nope");
        assert_eq!(expand(&settings(&["sh_test", "+sh_test"])).unwrap_err(), "Duplicated symbol 'sh_test' in --incompatible_autoload_externally");
    }

    #[test]
    fn rule_sets_own_repos_get_no_autoloads() {
        let autoloads = expand(&settings(&["sh_test"])).unwrap();
        assert!(!autoloads.in_bzl(RepoName::new("rules_shell+")));
        assert!(autoloads.in_bzl(RepoName::new("platforms")));
        assert!(!autoloads.in_bzl(RepoName::MAIN));
        assert!(!autoloads.in_build(RepoName::MAIN));
        assert!(autoloads.in_build(RepoName::new("rules_shell+")));
    }

    #[test]
    fn no_symbols_means_no_autoloads() {
        let autoloads = expand(&AutoloadSettings::default()).unwrap();
        assert!(!autoloads.enabled());
        assert_eq!(autoloads.bzl_context(RepoName::new("x+")), BzlContext::Plain);
    }
}
