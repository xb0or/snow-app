//! Theme 设置 NAPI 类型、From 转换与转发函数。

use super::*;

// ===== Theme settings NAPI 导出 =====

#[napi(object)]
pub struct ThemePaletteNapi {
    pub bg_primary: String,
    pub bg_secondary: String,
    pub bg_tertiary: String,
    pub bg_hover: String,
    pub bg_active: String,
    pub chrome_bg: String,
    pub app_bg: String,
    pub border_color: String,
    pub border_light: String,
    pub border_subtle: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub text_tertiary: String,
    pub text_muted: String,
    pub accent_green: String,
    pub accent_green_bg: String,
    pub accent_green_text: String,
    pub accent_red: String,
    pub accent_red_bg: String,
    pub accent_red_text: String,
    pub accent_blue: String,
    pub accent_blue_bg: String,
    pub accent_blue_text: String,
    pub accent_color: String,
    pub on_solid: String,
    pub selection_bg: String,
    pub focus_ring: String,
}

impl From<ThemePalette> for ThemePaletteNapi {
    fn from(p: ThemePalette) -> Self {
        ThemePaletteNapi {
            bg_primary: p.bg_primary,
            bg_secondary: p.bg_secondary,
            bg_tertiary: p.bg_tertiary,
            bg_hover: p.bg_hover,
            bg_active: p.bg_active,
            chrome_bg: p.chrome_bg,
            app_bg: p.app_bg,
            border_color: p.border_color,
            border_light: p.border_light,
            border_subtle: p.border_subtle,
            text_primary: p.text_primary,
            text_secondary: p.text_secondary,
            text_tertiary: p.text_tertiary,
            text_muted: p.text_muted,
            accent_green: p.accent_green,
            accent_green_bg: p.accent_green_bg,
            accent_green_text: p.accent_green_text,
            accent_red: p.accent_red,
            accent_red_bg: p.accent_red_bg,
            accent_red_text: p.accent_red_text,
            accent_blue: p.accent_blue,
            accent_blue_bg: p.accent_blue_bg,
            accent_blue_text: p.accent_blue_text,
            accent_color: p.accent_color,
            on_solid: p.on_solid,
            selection_bg: p.selection_bg,
            focus_ring: p.focus_ring,
        }
    }
}

impl From<ThemePaletteNapi> for ThemePalette {
    fn from(p: ThemePaletteNapi) -> Self {
        ThemePalette {
            bg_primary: p.bg_primary,
            bg_secondary: p.bg_secondary,
            bg_tertiary: p.bg_tertiary,
            bg_hover: p.bg_hover,
            bg_active: p.bg_active,
            chrome_bg: p.chrome_bg,
            app_bg: p.app_bg,
            border_color: p.border_color,
            border_light: p.border_light,
            border_subtle: p.border_subtle,
            text_primary: p.text_primary,
            text_secondary: p.text_secondary,
            text_tertiary: p.text_tertiary,
            text_muted: p.text_muted,
            accent_green: p.accent_green,
            accent_green_bg: p.accent_green_bg,
            accent_green_text: p.accent_green_text,
            accent_red: p.accent_red,
            accent_red_bg: p.accent_red_bg,
            accent_red_text: p.accent_red_text,
            accent_blue: p.accent_blue,
            accent_blue_bg: p.accent_blue_bg,
            accent_blue_text: p.accent_blue_text,
            accent_color: p.accent_color,
            on_solid: p.on_solid,
            selection_bg: p.selection_bg,
            focus_ring: p.focus_ring,
        }
    }
}

#[napi(object)]
pub struct CustomThemeNapi {
    pub light: ThemePaletteNapi,
    pub dark: ThemePaletteNapi,
}

impl From<CustomTheme> for CustomThemeNapi {
    fn from(c: CustomTheme) -> Self {
        CustomThemeNapi {
            light: c.light.into(),
            dark: c.dark.into(),
        }
    }
}

impl From<CustomThemeNapi> for CustomTheme {
    fn from(c: CustomThemeNapi) -> Self {
        CustomTheme {
            light: c.light.into(),
            dark: c.dark.into(),
        }
    }
}

#[napi(object)]
pub struct ThemeBackgroundNapi {
    pub enabled: bool,
    pub image_path: String,
    pub opacity: f64,
    pub blur: f64,
}

impl From<ThemeBackground> for ThemeBackgroundNapi {
    fn from(b: ThemeBackground) -> Self {
        ThemeBackgroundNapi {
            enabled: b.enabled,
            image_path: b.image_path,
            opacity: b.opacity,
            blur: b.blur,
        }
    }
}

impl From<ThemeBackgroundNapi> for ThemeBackground {
    fn from(b: ThemeBackgroundNapi) -> Self {
        ThemeBackground {
            enabled: b.enabled,
            image_path: b.image_path,
            opacity: b.opacity,
            blur: b.blur,
        }
    }
}

#[napi(object)]
pub struct ThemeStreamCursorNapi {
    pub icon_type: String,
    pub lucide_name: String,
    pub svg_path: String,
    pub icon_size: f64,
}

impl From<ThemeStreamCursor> for ThemeStreamCursorNapi {
    fn from(c: ThemeStreamCursor) -> Self {
        ThemeStreamCursorNapi {
            icon_type: c.icon_type,
            lucide_name: c.lucide_name,
            svg_path: c.svg_path,
            icon_size: c.icon_size,
        }
    }
}

impl From<ThemeStreamCursorNapi> for ThemeStreamCursor {
    fn from(c: ThemeStreamCursorNapi) -> Self {
        ThemeStreamCursor {
            icon_type: c.icon_type,
            lucide_name: c.lucide_name,
            svg_path: c.svg_path,
            icon_size: c.icon_size,
        }
    }
}

#[napi(object)]
pub struct ThemeSettingsNapi {
    pub mode: String,
    pub preset_id: String,
    pub custom: CustomThemeNapi,
    pub background: ThemeBackgroundNapi,
    pub font_family: String,
    pub stream_cursor: ThemeStreamCursorNapi,
}

impl From<ThemeSettings> for ThemeSettingsNapi {
    fn from(s: ThemeSettings) -> Self {
        ThemeSettingsNapi {
            mode: s.mode,
            preset_id: s.preset_id,
            custom: s.custom.into(),
            background: s.background.into(),
            font_family: s.font_family,
            stream_cursor: s.stream_cursor.into(),
        }
    }
}

impl From<ThemeSettingsNapi> for ThemeSettings {
    fn from(s: ThemeSettingsNapi) -> Self {
        ThemeSettings {
            mode: s.mode,
            preset_id: s.preset_id,
            custom: s.custom.into(),
            background: s.background.into(),
            font_family: s.font_family,
            stream_cursor: s.stream_cursor.into(),
        }
    }
}

#[napi]
pub async fn get_theme_settings() -> napi::Result<ThemeSettingsNapi> {
    let settings = tokio::task::spawn_blocking(crate::storage::get_theme_settings)
        .await
        .map_err(map_spawn_error)??;
    Ok(settings.into())
}

#[napi]
pub async fn set_theme_settings(settings: ThemeSettingsNapi) -> napi::Result<()> {
    let settings = settings.into();
    tokio::task::spawn_blocking(move || crate::storage::set_theme_settings(settings))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn save_theme_background_image(source_path: String) -> napi::Result<String> {
    tokio::task::spawn_blocking(move || crate::storage::save_theme_background_image(source_path))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_theme_background_image(image_path: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_theme_background_image(image_path))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn save_theme_stream_cursor_svg(source_path: String) -> napi::Result<String> {
    tokio::task::spawn_blocking(move || crate::storage::save_theme_stream_cursor_svg(source_path))
        .await
        .map_err(map_spawn_error)?
}

#[napi]
pub async fn delete_theme_stream_cursor_svg(svg_path: String) -> napi::Result<()> {
    tokio::task::spawn_blocking(move || crate::storage::delete_theme_stream_cursor_svg(svg_path))
        .await
        .map_err(map_spawn_error)?
}
