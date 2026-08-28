use std::rc::Rc;

use gpui::{App, SharedString};
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

fn slot(hex: &'static str) -> Option<SharedString> {
    Some(hex.into())
}

fn graphite_theme_colors() -> ThemeConfigColors {
    let mut colors = ThemeConfigColors::default();

    colors.background = slot(CANVAS);
    colors.foreground = slot(TEXT_BRIGHT);
    colors.border = slot(HAIRLINE);
    colors.ring = slot(RING);
    colors.caret = slot(TEXT_BRIGHT);
    colors.selection = slot(EDGE);
    colors.input = slot(INPUT_WELL);
    colors.popover = slot(SURFACE);
    colors.popover_foreground = slot(TEXT_BRIGHT);
    colors.accent = slot(HOVER);
    colors.accent_foreground = slot(TEXT_BRIGHT);
    colors.muted = slot(RAISED);
    colors.muted_foreground = slot(TEXT_DIM);
    colors.secondary = slot(RAISED);
    colors.secondary_hover = slot(RAISED_HOVER);
    colors.secondary_active = slot(RAISED_ACTIVE);
    colors.secondary_foreground = slot(TEXT_BRIGHT);
    colors.primary = slot(PRIMARY);
    colors.primary_hover = slot(PRIMARY_HOVER);
    colors.primary_active = slot(PRIMARY_ACTIVE);
    colors.primary_foreground = slot(PRIMARY_TEXT);
    colors.list_active = slot(RAISED);
    colors.list_active_border = slot(EDGE);
    colors.scrollbar_thumb = slot(SCROLLBAR_THUMB);
    colors.sidebar = slot(CHROME);
    colors.sidebar_border = slot(CHROME_HAIRLINE);
    colors.sidebar_foreground = slot(TEXT_BRIGHT);
    colors.sidebar_accent = slot(HOVER);
    colors.title_bar = slot(CHROME);
    colors.title_bar_border = slot(CHROME_HAIRLINE);
    colors.status_bar = slot(CHROME);
    colors.status_bar_border = slot(CHROME_HAIRLINE);
    colors.window_border = slot(CHROME_HAIRLINE);

    colors
}
