//! The environment repository rules and module extensions see: the
//! client's, with `--repo_env` on top.

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

    /// Set the environment, returning whether it changed, in which case
    /// [`RepoEnvKey`] must be invalidated.
    pub fn set(&self, environment: RepoEnvironment) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current == environment {
            return false;
        }
        *current = environment;
        true
    }

    fn get(&self) -> RepoEnvironment {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// The environment repository rules see.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoEnvKey;

impl Key for RepoEnvKey {
    type Value = RepoEnvironment;
    const KIND: &'static str = "REPO_ENV";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<RepoEnvironment, SkyError> {
        Ok(ctx.global::<ClientEnv>()?.get())
    }
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
