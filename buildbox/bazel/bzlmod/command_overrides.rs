//! The repositories and modules a command's options replace or add:
//! `--override_repository`, `--override_module`, `--inject_repository`
//! and `--ignore_dev_dependency`.

use crate::module_file::{ModuleFile, Override, RepoRuleCall};
use crate::star_value::StarValue;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::sync::{Arc, Mutex};

/// What the options give. Each path is absolute, and each name appears
/// once, with the last value the options gave it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct CommandOverrides {
    /// Repositories, by canonical name or by the apparent name the main
    /// repository sees them by, replaced by directories.
    pub repositories: Vec<(String, String)>,
    /// Modules, by name, replaced by directories, as `local_path_override`
    /// in the root module would replace them.
    pub modules: Vec<(String, String)>,
    /// Repositories added to the root module's, as `local_repository`.
    pub injections: Vec<(String, String)>,
    /// Whether the root module's dev dependencies are left out.
    pub ignore_dev_dependency: bool,
}

encoded_value!(CommandOverrides);

impl CommandOverrides {
    /// The directory a repository is replaced by, if its canonical name is
    /// overridden.
    pub fn repository(&self, canonical: &str) -> Option<&str> {
        self.repositories.iter().find(|(name, _)| name == canonical).map(|(_, path)| path.as_str())
    }

    /// The overrides naming repositories by apparent names, which name no
    /// canonical repository, as Bazel tells them apart.
    pub fn by_apparent_name(&self) -> impl Iterator<Item = (&str, &str)> {
        self.repositories.iter().filter(|(name, _)| !name.contains('+')).map(|(name, path)| (name.as_str(), path.as_str()))
    }

    /// The root module's overrides with the options' module overrides,
    /// which take precedence.
    pub fn module_overrides(&self, root: &ModuleFile) -> Vec<(String, Override)> {
        let mut overrides = root.overrides.clone();
        for (name, path) in &self.modules {
            overrides.retain(|(overridden, _)| overridden != name);
            overrides.push((name.clone(), Override::LocalPath { path: path.clone() }));
        }
        overrides
    }

    /// Add the injected repositories to the root module, as Bazel does: as
    /// `local_repository` repositories the root module imports. A name the
    /// root module already uses is an error.
    pub fn inject(&self, root: &mut ModuleFile) -> Result<(), String> {
        for (name, path) in &self.injections {
            let taken = root.deps.iter().any(|dep| dep.repo_name.as_deref() == Some(name.as_str())) || root.repo_rules.iter().any(|call| call.name == *name);
            if taken {
                return Err(format!("The repo name '{name}' is already being used by the root module, so it can't be added by --inject_repository"));
            }
            root.repo_rules.push(RepoRuleCall {
                bzl: "@bazel_tools//tools/build_defs/repo:local.bzl".to_string(),
                rule: "local_repository".to_string(),
                name: name.clone(),
                attrs: vec![("path".to_string(), StarValue::String(path.clone()))],
                dev: true,
            });
        }
        Ok(())
    }
}

/// A graph service: the overrides the latest command's options gave.
#[derive(Debug, Default)]
pub struct CommandOverridesService(Mutex<CommandOverrides>);

impl CommandOverridesService {
    pub fn new() -> Arc<CommandOverridesService> {
        Arc::default()
    }

    /// Set the overrides, returning whether they changed, in which case
    /// [`CommandOverridesKey`] must be invalidated.
    pub fn set(&self, overrides: CommandOverrides) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != overrides;
        *current = overrides;
        changed
    }
}

/// The overrides the latest command's options gave.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct CommandOverridesKey;

impl Key for CommandOverridesKey {
    type Value = CommandOverrides;
    const KIND: &'static str = "COMMAND_OVERRIDES";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<CommandOverrides, SkyError> {
        match ctx.global::<CommandOverridesService>() {
            Ok(service) => Ok(service.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()),
            Err(_) => Ok(CommandOverrides::default()),
        }
    }
}
