use super::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_CONFIG_FILE_NAME: &'static str = "buildbox.toml";

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address for the server to listen on.
    pub addr: String,

    /// Cache blob storage directory.
    pub cachedir: String,

    /// Execution directory.
    pub execdir: String,

    /// Whether to retain sandboxes after use
    #[serde(default)]
    pub retain_sandboxes: bool,
}

impl Config {
    pub fn load(path_override: Option<&PathBuf>) -> Result<Config> {
        let default_path = PathBuf::from(DEFAULT_CONFIG_FILE_NAME);
        let path = path_override.unwrap_or(&default_path);
        let content = std::fs::read_to_string(path).map_err(Error::io)?;

        let mut config: Config = toml::from_str(&content).map_err(Error::boxed)?;

        // Expands the "~" shell alias for the $HOME directory.
        config.cachedir = shellexpand::tilde(&config.cachedir).to_string();
        config.execdir = shellexpand::tilde(&config.execdir).to_string();

        Ok(config)
    }
}
