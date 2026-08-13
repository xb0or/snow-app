use std::path::Path;

use napi::bindgen_prelude::*;
use serde::{Deserialize, Serialize};

use super::{
    get_system_setting_value, set_system_setting, DEFAULT_PRIVACY_SETTING_CODE,
    DEFAULT_PRIVACY_SETTING_NAME,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrivacyApiConfig {
    pub url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrivacyToolResultsConfig {
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrivacySettings {
    pub enabled: bool,
    pub mode: String,
    pub api: PrivacyApiConfig,
    pub tool_results: PrivacyToolResultsConfig,
}

impl PrivacySettings {
    fn normalize(&mut self) {
        self.mode = self.mode.trim().to_string();
        if self.mode.is_empty() {
            self.mode = "local".to_string();
        }
        self.api.url = self.api.url.trim().to_string();
        self.api.api_key = self.api.api_key.trim().to_string();
        self.api.model = self.api.model.trim().to_string();
        if self.api.model.is_empty() {
            self.api.model = "openai/privacy-filter".to_string();
        }
        self.tool_results.tools = self
            .tool_results
            .tools
            .iter()
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect();
    }
}

pub fn get_privacy_settings(database_path: &Path) -> Result<PrivacySettings> {
    let Some(raw_value) = get_system_setting_value(database_path, DEFAULT_PRIVACY_SETTING_CODE)?
    else {
        return Ok(PrivacySettings::default());
    };

    let mut settings = serde_json::from_str::<PrivacySettings>(&raw_value).map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to parse privacy settings: {error}"),
        )
    })?;
    settings.normalize();
    Ok(settings)
}

pub fn set_privacy_settings(database_path: &Path, settings: &PrivacySettings) -> Result<()> {
    let mut normalized = settings.clone();
    normalized.normalize();
    let setting_value = serde_json::to_string(&normalized).map_err(|error| {
        Error::new(
            Status::GenericFailure,
            format!("Failed to serialize privacy settings: {error}"),
        )
    })?;
    set_system_setting(
        database_path,
        DEFAULT_PRIVACY_SETTING_NAME,
        DEFAULT_PRIVACY_SETTING_CODE,
        &setting_value,
    )
}
