use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use toml::Table;

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct Config {
    /// Path to the certificate public key.
    pub cert: PathBuf,

    /// Certificate private key path.
    pub key: PathBuf,

    /// Cache blob storage directory.
    pub cachedir: PathBuf,

    /// Execution directory.
    pub execdir: PathBuf,
}

impl Config {
    pub fn load(path: &PathBuf) -> Result<Config> {
        let content = std::fs::read_to_string(path).map_err(Error::io)?;
        toml::from_str(&content).map_err(Error::boxed)
    }
}
