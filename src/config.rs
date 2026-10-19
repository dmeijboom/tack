use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::error::{AppError, Result};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub context: HashMap<String, Context>,
    pub kubeconfig_dir: Option<PathBuf>,
    pub shell: Option<PathBuf>,
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
        account: Option<String>,
        configuration: Option<String>,
    },
    Aks {
        subscription: String,
        #[serde(rename = "resource-group")]
        resource_group: String,
        tenant: Option<String>,
    },
    Tcloud {
        organisation: String,
    },
}

pub fn load_config(filename: impl AsRef<Path>) -> Result<Config> {
    let path = filename.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| {
        let code = if source.kind() == std::io::ErrorKind::NotFound {
            "config_not_found"
        } else {
            "config_read_failed"
        };
        AppError::io(code, "Unable to read tack configuration", path, source).help(
            "Create or fix ~/.config/tack/config.toml; see the README for configuration examples.",
        )
    })?;
    toml::from_str(&content).map_err(|source: toml::de::Error| {
        let span = source.span();
        AppError::new("config_invalid", "Invalid tack configuration")
            .at_path(path)
            .help("Correct the highlighted TOML or context settings and retry.")
            .detail(source.message())
            .with_source_code(path, content, span)
    })
}

pub fn load_default_config() -> Result<Config> {
    let home = dirs::home_dir().ok_or_else(|| {
        AppError::new("home_unavailable", "Unable to determine the home directory")
            .help("Set HOME so tack can locate ~/.config/tack/config.toml.")
    })?;
    load_config(home.join(".config").join("tack").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_settings_are_optional() {
        let config: Config = toml::from_str(
            r#"
            [context.gke]
            generator = "gcloud"
            project = "project"
            location = "location"
            [context.aks]
            generator = "aks"
            subscription = "subscription"
            resource-group = "group"
            "#,
        )
        .unwrap();
        assert!(matches!(
            config.context["gke"].generator,
            Some(Generator::Gcloud {
                account: None,
                configuration: None,
                ..
            })
        ));
        assert!(matches!(
            config.context["aks"].generator,
            Some(Generator::Aks { tenant: None, .. })
        ));
    }
}
