use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{host::Result, permissions::Mode};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub permissions: Option<Mode>,
    pub diff_tool: Option<String>,
    pub diff_viewer: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub models: BTreeMap<String, String>,
}

impl Provider {
    pub fn key(&self, name: &str) -> Result<(String, String)> {
        if let Some(variable) = &self.api_key_env {
            return std::env::var(variable)
                .ok()
                .filter(|key| !key.trim().is_empty())
                .map(|key| (key, variable.clone()))
                .ok_or_else(|| format!("provider {name}: {variable} is not set").into());
        }
        self.api_key
            .clone()
            .filter(|key| !key.trim().is_empty())
            .map(|key| (key, "config api_key".to_owned()))
            .ok_or_else(|| format!("provider {name} needs api_key or api_key_env").into())
    }
}

impl Config {
    pub fn path(home: &Path) -> PathBuf {
        home.join("config.toml")
    }

    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display()).into()),
        };
        let config: Self =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        for (name, provider) in &config.providers {
            if provider.models.is_empty() {
                return Err(format!("{}: provider {name} lists no models", path.display()).into());
            }
            if provider.api_key.is_some() && provider.api_key_env.is_some() {
                return Err(format!(
                    "{}: provider {name} sets both api_key and api_key_env",
                    path.display()
                )
                .into());
            }
        }
        Ok(config)
    }

    pub fn provider_for(&self, model: &str) -> Option<(&str, &Provider)> {
        let model = model.trim();
        self.providers
            .iter()
            .find(|(_, provider)| provider.models.contains_key(model))
            .map(|(name, provider)| (name.as_str(), provider))
    }
}
