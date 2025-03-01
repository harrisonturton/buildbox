use super::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_CONFIG_FILE_NAME: &'static str = "buildbox.toml";

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address for the server to listen on.
    pub addr: String,

    /// Path to the certificate public key.
    pub cert: PathBuf,

    /// Certificate private key path.
    pub key: PathBuf,

    /// Cache blob storage directory.
    pub cachedir: PathBuf,

    /// Execution directory.
    pub execdir: PathBuf,

    /// Whether to retain sandboxes after use
    #[serde(default)]
    pub retain_sandboxes: bool,
}

impl Config {
    pub fn load(path_override: Option<&PathBuf>) -> Result<Config> {
        let default_path = PathBuf::from(DEFAULT_CONFIG_FILE_NAME);
        let path = path_override.unwrap_or(&default_path);
        let content = std::fs::read_to_string(path).map_err(Error::io)?;
        toml::from_str(&content).map_err(Error::boxed)
    }
}
