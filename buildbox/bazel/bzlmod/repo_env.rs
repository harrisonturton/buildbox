//! The environment repository rules and module extensions see: the
//! client's, with `--repo_env` on top.
//!
//! As in Bazel, a rule or extension is run again only when a variable it
//! declares with `environ`, or a `--repo_env` value, changes: each of those
//! is a key of its own, [`RepoEnvVarKey`]. What else they read of the
//! environment, such as `PATH` for `execute`, is the latest command's,
//! without depending on it.

use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::sync::{Arc, Mutex};

/// The environment a workspace's latest command gave.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct RepoEnvironment {
    /// Every variable, sorted by name.
    pub env: Vec<(String, String)>,
    /// The `--repo_env` variables, which identify the repositories they're
    /// fetched with, sorted by name.
    pub overrides: Vec<(String, String)>,
}

encoded_value!(RepoEnvironment);

impl RepoEnvironment {
    /// The client's environment with `overrides` on top. An override
    /// without `=` takes the client's value.
    pub fn new(client: impl IntoIterator<Item = (String, String)>, overrides: &[&str]) -> RepoEnvironment {
        let mut env: Vec<(String, String)> = client.into_iter().collect();
        let mut set = Vec::new();
        for entry in overrides {
            let (name, value) = match entry.split_once('=') {
                Some((name, value)) => (name.to_string(), value.to_string()),
                None => (entry.to_string(), env.iter().find(|(name, _)| name.as_str() == *entry).map(|(_, value)| value.clone()).unwrap_or_default()),
            };
            env.retain(|(existing, _)| *existing != name);
            set.retain(|(existing, _): &(String, String)| *existing != name);
            env.push((name.clone(), value.clone()));
            set.push((name, value));
        }
        env.sort();
        set.sort();
        RepoEnvironment { env, overrides: set }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// A graph service: the environment of the workspace's latest command.
#[derive(Default)]
pub struct ClientEnv(Mutex<RepoEnvironment>);

impl ClientEnv {
    pub fn new() -> Arc<ClientEnv> {
        Arc::new(ClientEnv::default())
    }

    /// Set the environment, returning what changed: the names of the
    /// variables whose values differ, and whether the `--repo_env` set
    /// did. The keys they name must be invalidated.
    pub fn set(&self, environment: RepoEnvironment) -> EnvChanges {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut names: Vec<String> = Vec::new();
        for (name, value) in &environment.env {
            if current.get(name) != Some(value.as_str()) {
                names.push(name.clone());
            }
        }
        for (name, _) in &current.env {
            if environment.get(name).is_none() {
                names.push(name.clone());
            }
        }
        let overrides = current.overrides != environment.overrides;
        *current = environment;
        EnvChanges { names, overrides }
    }

    /// The latest command's environment, without depending on it.
    pub fn current(&self) -> RepoEnvironment {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// What changed in the environment between commands.
#[derive(Debug, Default)]
pub struct EnvChanges {
    pub names: Vec<String>,
    pub overrides: bool,
}

/// The `--repo_env` variables, which identify the repositories they're
/// fetched with.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoEnvKey;

/// The `--repo_env` variables, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct RepoEnvOverrides(pub Vec<(String, String)>);

encoded_value!(RepoEnvOverrides);

impl Key for RepoEnvKey {
    type Value = RepoEnvOverrides;
    const KIND: &'static str = "REPO_ENV";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<RepoEnvOverrides, SkyError> {
        Ok(RepoEnvOverrides(ctx.global::<ClientEnv>()?.current().overrides))
    }
}

/// One variable of the environment repository rules see.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoEnvVarKey(pub String);

/// A variable's value, if it's set.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RepoEnvVar(pub Option<String>);

encoded_value!(RepoEnvVar);

impl Key for RepoEnvVarKey {
    type Value = RepoEnvVar;
    const KIND: &'static str = "REPO_ENV_VAR";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<RepoEnvVar, SkyError> {
        Ok(RepoEnvVar(ctx.global::<ClientEnv>()?.current().get(&self.0).map(str::to_string)))
    }
}

/// The values of `names`, depending on each, and on the `--repo_env` set:
/// what a rule or extension that declares `names` is run again for.
pub async fn declared(ctx: &Ctx, names: &[String]) -> Result<String, SkyError> {
    let overrides = ctx.compute(&RepoEnvKey).await?;
    let keys: Vec<RepoEnvVarKey> = names.iter().cloned().map(RepoEnvVarKey).collect();
    let mut inputs = format!("{:?}", overrides.0);
    for (name, value) in names.iter().zip(ctx.compute_many(&keys).await) {
        inputs.push_str(&format!("\n{name}={:?}", value?.0));
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_replace_the_clients_variables() {
        let client = [("CC".to_string(), "gcc".to_string()), ("HOME".to_string(), "/h".to_string())];
        let environment = RepoEnvironment::new(client, &["CC=clang", "HOME"]);
        assert_eq!(environment.get("CC"), Some("clang"));
        assert_eq!(environment.overrides, [("CC".to_string(), "clang".to_string()), ("HOME".to_string(), "/h".to_string())]);
    }
}
