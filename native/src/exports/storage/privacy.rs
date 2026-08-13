//! 隐私设置（隐私模式 API 配置）的 NAPI 类型与转发。

use super::*;

#[napi(object)]
pub struct PrivacyApiConfigNapi {
    pub url: String,
    pub api_key: String,
    pub model: String,
}

#[napi(object)]
pub struct PrivacyToolResultsConfigNapi {
    pub tools: Vec<String>,
}

#[napi(object)]
pub struct PrivacySettingsNapi {
    pub enabled: bool,
    pub mode: String,
    pub api: PrivacyApiConfigNapi,
    pub tool_results: PrivacyToolResultsConfigNapi,
}

impl From<PrivacySettings> for PrivacySettingsNapi {
    fn from(settings: PrivacySettings) -> Self {
        PrivacySettingsNapi {
            enabled: settings.enabled,
            mode: settings.mode,
            api: PrivacyApiConfigNapi {
                url: settings.api.url,
                api_key: settings.api.api_key,
                model: settings.api.model,
            },
            tool_results: PrivacyToolResultsConfigNapi {
                tools: settings.tool_results.tools,
            },
        }
    }
}

impl From<PrivacySettingsNapi> for PrivacySettings {
    fn from(settings: PrivacySettingsNapi) -> Self {
        PrivacySettings {
            enabled: settings.enabled,
            mode: settings.mode,
            api: PrivacyApiConfig {
                url: settings.api.url,
                api_key: settings.api.api_key,
                model: settings.api.model,
            },
            tool_results: PrivacyToolResultsConfig {
                tools: settings.tool_results.tools,
            },
        }
    }
}

#[napi]
pub async fn get_privacy_settings() -> napi::Result<PrivacySettingsNapi> {
    let settings = tokio::task::spawn_blocking(crate::storage::get_privacy_settings)
        .await
        .map_err(map_spawn_error)??;
    Ok(settings.into())
}

#[napi]
pub async fn set_privacy_settings(settings: PrivacySettingsNapi) -> napi::Result<()> {
    let settings = settings.into();
    tokio::task::spawn_blocking(move || crate::storage::set_privacy_settings(settings))
        .await
        .map_err(map_spawn_error)?
}
