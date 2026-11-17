use std::ffi::OsStr;
use std::time::Duration;

use ratatui::style::Color;

/// How long to wait for the terminal to answer a background-color query.
/// Terminals that don't support the query are usually detected well before this.
const BACKGROUND_QUERY_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// Detects whether the terminal has a light or dark background.
///
/// Precedence: explicit CLI flag, `REB_THEME_MODE`, querying the terminal's
/// background color (OSC 11), `COLORFGBG`, then dark as a last resort.
pub fn detect_theme_mode(cli_override: Option<ThemeMode>) -> ThemeMode {
    detect_theme_mode_with(
        cli_override,
        std::env::var_os("REB_THEME_MODE").as_deref(),
        query_terminal_theme_mode,
        std::env::var_os("COLORFGBG").as_deref(),
    )
}

pub fn detect_theme_mode_with(
    cli_override: Option<ThemeMode>,
    env_override: Option<&OsStr>,
    query_terminal: impl FnOnce() -> Option<ThemeMode>,
    colorfgbg: Option<&OsStr>,
) -> ThemeMode {
    cli_override
        .or_else(|| env_override.and_then(parse_theme_mode_override))
        .or_else(query_terminal)
        .or_else(|| colorfgbg.and_then(parse_colorfgbg_theme_mode))
        .unwrap_or(ThemeMode::Dark)
}

fn query_terminal_theme_mode() -> Option<ThemeMode> {
    let mut options = terminal_colorsaurus::QueryOptions::default();
    options.timeout = BACKGROUND_QUERY_TIMEOUT;
    match terminal_colorsaurus::theme_mode(options).ok()? {
        terminal_colorsaurus::ThemeMode::Light => Some(ThemeMode::Light),
        terminal_colorsaurus::ThemeMode::Dark => Some(ThemeMode::Dark),
    }
}

fn parse_theme_mode_override(value: &OsStr) -> Option<ThemeMode> {
    match value.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "light" => Some(ThemeMode::Light),
        "dark" => Some(ThemeMode::Dark),
        _ => None,
    }
}

fn parse_colorfgbg_theme_mode(value: &OsStr) -> Option<ThemeMode> {
    let value = value.to_string_lossy();
    let last = value.rsplit(';').next()?.trim();
    let bg_index: u8 = last.parse().ok()?;
    Some(if bg_index <= 6 || bg_index == 8 {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    })
}

/// Every UI color reb draws with, so light and dark terminals each get a set
/// that is readable on their background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Body text; `Reset` follows the terminal's own foreground.
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub highlight: Color,
    pub marker: Color,
    pub border_inactive: Color,
    pub comment_border: Color,
    pub line_comment_border: Color,
    pub file_comment_border: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub selected_unfocused_fg: Color,
    pub selected_unfocused_bg: Color,
    pub added: Color,
    pub deleted: Color,
    pub modified: Color,
    pub renamed: Color,
    pub type_changed: Color,
    pub copied: Color,
    pub unknown: Color,
    pub context_sign: Color,
    pub add_bg: Color,
    pub delete_bg: Color,
    pub cursor_bg: Color,
    pub selection_bg: Color,
}

impl Palette {
    pub const DARK: Self = Self {
        text: Color::Reset,
        muted: Color::DarkGray,
        accent: Color::Rgb(160, 196, 255),
        highlight: Color::Rgb(231, 193, 119),
        marker: Color::Yellow,
        border_inactive: Color::Rgb(70, 74, 90),
        comment_border: Color::Rgb(110, 130, 170),
        line_comment_border: Color::Rgb(216, 180, 84),
        file_comment_border: Color::Rgb(113, 205, 205),
        selected_fg: Color::Black,
        selected_bg: Color::Rgb(208, 221, 255),
        selected_unfocused_fg: Color::Rgb(225, 232, 244),
        selected_unfocused_bg: Color::Rgb(45, 54, 70),
        added: Color::Rgb(149, 198, 136),
        deleted: Color::Rgb(224, 110, 110),
        modified: Color::Rgb(160, 196, 255),
        renamed: Color::Rgb(231, 193, 119),
        type_changed: Color::Rgb(214, 180, 255),
        copied: Color::Rgb(113, 205, 205),
        unknown: Color::Gray,
        context_sign: Color::Rgb(133, 146, 178),
        add_bg: Color::Rgb(33, 58, 43),
        delete_bg: Color::Rgb(74, 34, 29),
        cursor_bg: Color::Rgb(52, 70, 91),
        selection_bg: Color::Rgb(37, 51, 68),
    };

    pub const LIGHT: Self = Self {
        text: Color::Reset,
        muted: Color::Rgb(110, 118, 129),
        accent: Color::Rgb(9, 105, 218),
        highlight: Color::Rgb(154, 103, 0),
        marker: Color::Rgb(176, 120, 0),
        border_inactive: Color::Rgb(190, 196, 204),
        comment_border: Color::Rgb(84, 110, 160),
        line_comment_border: Color::Rgb(176, 120, 0),
        file_comment_border: Color::Rgb(27, 124, 131),
        selected_fg: Color::Rgb(31, 35, 40),
        selected_bg: Color::Rgb(190, 214, 252),
        selected_unfocused_fg: Color::Rgb(31, 35, 40),
        selected_unfocused_bg: Color::Rgb(226, 231, 238),
        added: Color::Rgb(26, 127, 55),
        deleted: Color::Rgb(207, 34, 46),
        modified: Color::Rgb(9, 105, 218),
        renamed: Color::Rgb(154, 103, 0),
        type_changed: Color::Rgb(130, 80, 223),
        copied: Color::Rgb(27, 124, 131),
        unknown: Color::Rgb(110, 118, 129),
        context_sign: Color::Rgb(140, 149, 159),
        add_bg: Color::Rgb(218, 251, 225),
        delete_bg: Color::Rgb(255, 235, 233),
        cursor_bg: Color::Rgb(200, 220, 250),
        selection_bg: Color::Rgb(228, 238, 252),
    };

    pub fn for_mode(mode: ThemeMode) -> &'static Self {
        match mode {
            ThemeMode::Light => &Self::LIGHT,
            ThemeMode::Dark => &Self::DARK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_query() -> Option<ThemeMode> {
        None
    }

    #[test]
    fn defaults_to_dark_when_nothing_is_known() {
        assert_eq!(
            detect_theme_mode_with(None, None, no_query, None),
            ThemeMode::Dark
        );
    }

    #[test]
    fn cli_override_wins_without_querying_terminal() {
        let mode = detect_theme_mode_with(
            Some(ThemeMode::Light),
            Some(OsStr::new("dark")),
            || panic!("terminal should not be queried"),
            Some(OsStr::new("15;0")),
        );
        assert_eq!(mode, ThemeMode::Light);
    }

    #[test]
    fn env_override_beats_terminal_query() {
        let mode = detect_theme_mode_with(
            None,
            Some(OsStr::new(" Light ")),
            || panic!("terminal should not be queried"),
            None,
        );
        assert_eq!(mode, ThemeMode::Light);
    }

    #[test]
    fn unrecognized_env_override_falls_through_to_query() {
        let mode = detect_theme_mode_with(
            None,
            Some(OsStr::new("auto")),
            || Some(ThemeMode::Light),
            None,
        );
        assert_eq!(mode, ThemeMode::Light);
    }

    #[test]
    fn terminal_query_beats_colorfgbg() {
        let mode = detect_theme_mode_with(
            None,
            None,
            || Some(ThemeMode::Light),
            Some(OsStr::new("15;0")),
        );
        assert_eq!(mode, ThemeMode::Light);
    }

    #[test]
    fn falls_back_to_colorfgbg_when_query_fails() {
        assert_eq!(
            detect_theme_mode_with(None, None, no_query, Some(OsStr::new("15;0"))),
            ThemeMode::Dark
        );
        assert_eq!(
            detect_theme_mode_with(None, None, no_query, Some(OsStr::new("0;15"))),
            ThemeMode::Light
        );
        assert_eq!(
            detect_theme_mode_with(None, None, no_query, Some(OsStr::new("0;default;15"))),
            ThemeMode::Light
        );
    }

    #[test]
    fn palette_for_mode_picks_matching_set() {
        assert_eq!(Palette::for_mode(ThemeMode::Light), &Palette::LIGHT);
        assert_eq!(Palette::for_mode(ThemeMode::Dark), &Palette::DARK);
    }

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("expected an RGB color, got {color:?}");
        };
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn light_palette_foregrounds_are_readable_on_white() {
        let white = Color::Rgb(255, 255, 255);
        let p = Palette::LIGHT;
        for (name, color) in [
            ("muted", p.muted),
            ("accent", p.accent),
            ("highlight", p.highlight),
            ("added", p.added),
            ("deleted", p.deleted),
            ("modified", p.modified),
            ("renamed", p.renamed),
            ("type_changed", p.type_changed),
            ("copied", p.copied),
        ] {
            let ratio = contrast(color, white);
            assert!(ratio >= 3.0, "{name} contrast on white is {ratio:.2}");
        }
    }

    #[test]
    fn light_palette_diff_backgrounds_stay_light() {
        let p = Palette::LIGHT;
        for (name, color) in [
            ("add_bg", p.add_bg),
            ("delete_bg", p.delete_bg),
            ("cursor_bg", p.cursor_bg),
            ("selection_bg", p.selection_bg),
            ("selected_bg", p.selected_bg),
        ] {
            assert!(
                luminance(color) > 0.6,
                "{name} is too dark for a light theme"
            );
        }
        assert!(
            contrast(p.selected_fg, p.selected_bg) >= 7.0,
            "focused selection text must stay readable"
        );
    }
}
