use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load config: {0}")]
    IO(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("config not found")]
    Missing,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub context: HashMap<String, Context>,
    pub kubeconfig_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct Context {
    #[serde(flatten)]
    pub generator: Option<Generator>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "generator", rename_all = "lowercase")]
pub enum Generator {
    Gcloud {
        project: String,
        location: String,
    },
    Aks {
        name: String,
        #[serde(rename = "resource-group")]
        resource_group: String,
    },
}

pub fn load_config(filename: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let content = std::fs::read(filename)?;
    Ok(toml::from_slice(&content)?)
}

pub fn load_default_config() -> Result<Config, ConfigError> {
    if let Some(home) = std::env::home_dir() {
        let path = home.join(".config").join("tack").join("config.toml");

        if path.exists() {
            return load_config(path);
        }
    }

    Err(ConfigError::Missing)
}
