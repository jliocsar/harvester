use std::rc::Rc;

use gpui::App;
use gpui_component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};

const GRAPHITE_THEME_NAME: &str = "Graphite Dark";

const CHROME: &str = "#111113";
const CANVAS: &str = "#161618";
const SURFACE: &str = "#1d1d20";
const HOVER: &str = "#222225";
const INPUT_WELL: &str = "#242428";
const RAISED: &str = "#27272b";
const RAISED_HOVER: &str = "#2e2e33";
const RAISED_ACTIVE: &str = "#35353b";
const HAIRLINE: &str = "#2c2c30";
const CHROME_HAIRLINE: &str = "#1f1f23";
const EDGE: &str = "#494950";
const RING: &str = "#61616b";
const SCROLLBAR_THUMB: &str = "#44444b";
const TEXT_DIM: &str = "#9c9ca5";
const TEXT_BRIGHT: &str = "#efeff0";
const PRIMARY: &str = "#ececee";
const PRIMARY_HOVER: &str = "#f7f7f8";
const PRIMARY_ACTIVE: &str = "#dadadd";
const PRIMARY_TEXT: &str = "#18181b";

const UI_FONT: &str = "Inter";
const MONO_FONT: &str = "Geist Mono";

const BASE_FONT_SIZE: f32 = 14.;

const CONTROL_RADIUS: usize = 6;
const PANEL_RADIUS: usize = 10;

pub fn install_graphite_theme(cx: &mut App) {
    let config = Rc::new(graphite_theme_config());
    let theme = Theme::global_mut(cx);

    theme.dark_theme = config.clone();
    theme.apply_config(&config);
}

fn graphite_theme_config() -> ThemeConfig {
    ThemeConfig {
        name: GRAPHITE_THEME_NAME.into(),
        mode: ThemeMode::Dark,
        radius: Some(CONTROL_RADIUS),
        radius_lg: Some(PANEL_RADIUS),
        font_size: Some(BASE_FONT_SIZE),
        font_family: Some(UI_FONT.into()),
        mono_font_size: Some(BASE_FONT_SIZE),
        mono_font_family: Some(MONO_FONT.into()),
        colors: graphite_theme_colors(),
        ..ThemeConfig::default()
    }
}

fn graphite_theme_colors() -> ThemeConfigColors {
    let mut colors = ThemeConfigColors::default();

    colors.background = Some(CANVAS.into());
    colors.foreground = Some(TEXT_BRIGHT.into());
    colors.border = Some(HAIRLINE.into());
    colors.ring = Some(RING.into());
    colors.caret = Some(TEXT_BRIGHT.into());
    colors.selection = Some(EDGE.into());
    colors.input = Some(INPUT_WELL.into());
    colors.popover = Some(SURFACE.into());
    colors.popover_foreground = Some(TEXT_BRIGHT.into());
    colors.accent = Some(HOVER.into());
    colors.accent_foreground = Some(TEXT_BRIGHT.into());
    colors.muted = Some(RAISED.into());
    colors.muted_foreground = Some(TEXT_DIM.into());
    colors.secondary = Some(RAISED.into());
    colors.secondary_hover = Some(RAISED_HOVER.into());
    colors.secondary_active = Some(RAISED_ACTIVE.into());
    colors.secondary_foreground = Some(TEXT_BRIGHT.into());
    colors.primary = Some(PRIMARY.into());
    colors.primary_hover = Some(PRIMARY_HOVER.into());
    colors.primary_active = Some(PRIMARY_ACTIVE.into());
    colors.primary_foreground = Some(PRIMARY_TEXT.into());
    colors.list_active = Some(RAISED.into());
    colors.list_active_border = Some(EDGE.into());
    colors.scrollbar_thumb = Some(SCROLLBAR_THUMB.into());
    colors.sidebar = Some(CHROME.into());
    colors.sidebar_border = Some(CHROME_HAIRLINE.into());
    colors.sidebar_foreground = Some(TEXT_BRIGHT.into());
    colors.sidebar_accent = Some(HOVER.into());
    colors.title_bar = Some(CHROME.into());
    colors.title_bar_border = Some(CHROME_HAIRLINE.into());
    colors.status_bar = Some(CHROME.into());
    colors.status_bar_border = Some(CHROME_HAIRLINE.into());
    colors.window_border = Some(CHROME_HAIRLINE.into());

    colors
}
