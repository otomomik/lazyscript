// 設定ファイル。~/.config/lazyscript/setting.json（JSON）を読み込む。
// 未指定の項目は既定値。パース失敗時は全部既定値で動く。

use std::path::PathBuf;
use std::str::FromStr;

use ratatui::style::Color;
use serde::Deserialize;

/// ヤンク先のクリップボード。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClipboardSink {
    System,
    Osc52,
    Both,
}

/// 配色。
#[derive(Clone, Copy)]
pub struct Theme {
    pub cursor: Color,
    pub search: Color,
    pub running: Color,
    pub failed: Color,
    pub focus: Color,
    /// 選択（Scripts の選択行・copy-mode の選択範囲）の背景色。
    /// None なら反転表示（従来の見た目）。
    pub selection: Option<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            cursor: Color::Yellow,
            search: Color::Cyan,
            running: Color::Green,
            failed: Color::Red,
            focus: Color::Cyan,
            selection: None,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub theme: Theme,
    pub clipboard: ClipboardSink,
    /// プロセスごとに保持するログ行数（vt100 スクロールバック長）。
    pub scrollback: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            clipboard: ClipboardSink::Both,
            scrollback: 10_000,
        }
    }
}

/// 設定を読み込む。ファイルが無い/壊れている場合は既定値。
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    serde_json::from_str::<RawConfig>(&text)
        .map(RawConfig::into_config)
        .unwrap_or_default()
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lazyscript").join("setting.json"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    theme: RawTheme,
    clipboard: Option<String>,
    /// ログの表示可能行数。
    scrollback: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTheme {
    cursor: Option<String>,
    search: Option<String>,
    running: Option<String>,
    failed: Option<String>,
    focus: Option<String>,
    selection: Option<String>,
}

impl RawConfig {
    fn into_config(self) -> Config {
        let d = Theme::default();
        let theme = Theme {
            cursor: color_or(self.theme.cursor, d.cursor),
            search: color_or(self.theme.search, d.search),
            running: color_or(self.theme.running, d.running),
            failed: color_or(self.theme.failed, d.failed),
            focus: color_or(self.theme.focus, d.focus),
            selection: self
                .theme
                .selection
                .as_deref()
                .and_then(|s| Color::from_str(s).ok()),
        };
        let clipboard = match self.clipboard.as_deref() {
            Some("system") => ClipboardSink::System,
            Some("osc52") => ClipboardSink::Osc52,
            _ => ClipboardSink::Both,
        };
        let scrollback = self.scrollback.filter(|&n| n > 0).unwrap_or(10_000);
        Config {
            theme,
            clipboard,
            scrollback,
        }
    }
}

/// "red" / "#00ff00" / "42" などをパース。失敗時は既定色。
fn color_or(value: Option<String>, default: Color) -> Color {
    value
        .as_deref()
        .and_then(|s| Color::from_str(s).ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_theme_clipboard_scrollback() {
        let json = r##"{
            "theme": { "cursor": "#ff0000", "running": "blue" },
            "clipboard": "system",
            "scrollback": 500
        }"##;
        let cfg = serde_json::from_str::<RawConfig>(json)
            .unwrap()
            .into_config();
        assert_eq!(cfg.theme.cursor, Color::Rgb(255, 0, 0));
        assert_eq!(cfg.theme.running, Color::Blue);
        // 未指定はデフォルト維持。
        assert_eq!(cfg.theme.search, Theme::default().search);
        assert!(matches!(cfg.clipboard, ClipboardSink::System));
        assert_eq!(cfg.scrollback, 500);
    }

    #[test]
    fn empty_json_yields_defaults() {
        let cfg = serde_json::from_str::<RawConfig>("{}")
            .unwrap()
            .into_config();
        assert_eq!(cfg.scrollback, 10_000);
        assert!(matches!(cfg.clipboard, ClipboardSink::Both));
        assert_eq!(cfg.theme.cursor, Color::Yellow);
    }

    #[test]
    fn invalid_color_falls_back() {
        let json = r#"{ "theme": { "cursor": "not-a-color" } }"#;
        let cfg = serde_json::from_str::<RawConfig>(json)
            .unwrap()
            .into_config();
        assert_eq!(cfg.theme.cursor, Color::Yellow);
    }
}
