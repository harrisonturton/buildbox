use super::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::c_char, path::PathBuf};

const DEFAULT_CONFIG_FILE_NAME: &'static str = "buildbox.toml";

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address for the server to listen on.
    pub addr: String,

    /// Cache blob storage directory.
    pub storage_dir: String,

    /// Execution directory.
    pub sandbox_dir: String,

    /// Whether to retain sandboxes after use
    #[serde(default)]
    pub retain_sandboxes: bool,
}

impl Config {
    pub fn load(path_override: Option<&PathBuf>) -> Result<Config> {
        let default_path = PathBuf::from(DEFAULT_CONFIG_FILE_NAME);
        let path = path_override.unwrap_or(&default_path);

        let config = if path.exists() {
            Self::load_from_path(path)?
        } else {
            Self::create_default()
        };

        // Expands the "~" shell alias for the $HOME directory.
        let storage_dir = shellexpand::tilde(&config.storage_dir).to_string();
        let sandbox_dir = shellexpand::tilde(&config.sandbox_dir).to_string();

        // Create the directories if they don't already exist.
        std::fs::create_dir_all(&storage_dir).map_err(Error::io)?;
        std::fs::create_dir_all(&sandbox_dir).map_err(Error::io)?;
        tracing::info!("Using storage directory: {storage_dir}");
        tracing::info!("Using sandbox directory: {sandbox_dir}");

        Ok(config)
    }

    fn load_from_path(path: &PathBuf) -> Result<Config> {
        let content = std::fs::read_to_string(path).map_err(Error::io)?;
        toml::from_str(&content).map_err(Error::boxed)
    }

    fn create_default() -> Config {
        Config {
            addr: "[::1]:50051".to_string(),
            storage_dir: "~/.buildbox/storage".to_string(),
            sandbox_dir: "~/.buildbox/sandbox".to_string(),
            retain_sandboxes: false,
        }
    }
}