//! The on-disk config contract. Mirrors how `state.rs` owns the state contract:
//! this module is the single source of truth for user-tunable settings. Every field
//! has a `Default` matching Hydra's built-in constant, so a missing or partial config
//! yields today's exact behavior. Loading (IO) is thin; parsing/merge is pure.
//!
//! (`ratatui::style::Color` and `std::path::PathBuf` are imported in the tasks that
//! first use them — Task 2 and Task 3 respectively.)

use ratatui::style::Color;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub timings: Timings,
    pub agent: Agent,
    pub popup: Popup,
    pub theme: Theme,
    pub alerts: Alerts,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Timings {
    pub stale_after_secs: u64,
    pub refresh_ms: u64,
    pub dirty_ttl_secs: u64,
    pub worktree_list_ttl_secs: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            stale_after_secs: 900,
            refresh_ms: 250,
            dirty_ttl_secs: 3,
            worktree_list_ttl_secs: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Agent {
    pub command: String,
    pub worktree_root: String,
    /// How a newly-started agent is laid out: `"window"` (default) or `"session"`.
    /// Interpreted via `Config::spawn_mode()`; unknown values behave as `"window"`.
    pub spawn_mode: String,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            command: "claude".to_string(),
            worktree_root: "~/work/tree".to_string(),
            spawn_mode: "window".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Popup {
    pub key: String,
    pub width: String,
    pub height: String,
}

impl Default for Popup {
    fn default() -> Self {
        Self {
            key: "a".to_string(),
            width: "70%".to_string(),
            height: "60%".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub tui: ThemeTui,
    pub status: ThemeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThemeTui {
    pub highlight_bg: String,
    pub working: String,
    pub needs_input: String,
    pub idle: String,
    pub unknown: String,
    /// Shortcut keys in the footer keybar (e.g. `j/k`, `⏎`, `a`).
    pub footer_key: String,
    /// The descriptions next to each footer key (e.g. `move`, `start/jump`).
    pub footer_label: String,
    /// Repo group headers (`▸ name`).
    pub header: String,
    /// Branch names in agent rows.
    pub branch: String,
    /// The uncommitted-change count (`Δ3`).
    pub dirty: String,
    /// Idle-worktree rows (glyph and `start ⏎` affordance).
    pub worktree_row: String,
}

impl Default for ThemeTui {
    fn default() -> Self {
        Self {
            highlight_bg: "#32323c".to_string(), // Rgb(50,50,60)
            working: "green".to_string(),
            needs_input: "yellow".to_string(),
            idle: "gray".to_string(),
            unknown: "darkgray".to_string(),
            footer_key: "green".to_string(),
            footer_label: "gray".to_string(),
            header: "blue".to_string(),
            branch: "cyan".to_string(),
            dirty: "magenta".to_string(),
            worktree_row: "darkgray".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThemeStatus {
    pub label: String,
    pub working: String,
    pub idle: String,
    pub alert_fg: String,
    pub alert_bg: String,
    pub unknown: String,
}

impl Default for ThemeStatus {
    fn default() -> Self {
        Self {
            label: "#b35b79".to_string(),    // rose
            working: "#5e857a".to_string(),  // teal
            idle: "#d9a594".to_string(),     // peach
            alert_fg: "#f2ecbc".to_string(), // cream
            alert_bg: "#d7474b".to_string(), // red
            unknown: "#b35b79".to_string(),  // rose
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Alerts {
    pub enabled: bool,
}

impl Default for Alerts {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// How Hydra lays out a newly-started agent. `Window` (default) opens one tmux window
/// in the current session; `Session` opens a dedicated session with a shell window and
/// an agent window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    Window,
    Session,
}

impl Config {
    /// Parse TOML into a `Config`, filling missing fields from defaults. On a syntax or
    /// type error, fall back to all defaults (the config must never break Hydra).
    /// Production call sites now go through `parse_reporting`/`load_reporting` directly,
    /// so this is only reachable from tests (hence the `allow`); kept as the simple,
    /// notice-free entry point the tests exercise.
    #[allow(dead_code)]
    pub fn parse(toml_str: &str) -> Config {
        Self::parse_reporting(toml_str).0
    }

    /// Like `parse`, but also reports whether parsing FAILED. On a syntax/type error
    /// returns `(Config::default(), true)`; valid or empty input returns `(cfg, false)`.
    /// Lets the TUI surface a notice while the silent paths keep using `parse`/`load`.
    pub fn parse_reporting(toml_str: &str) -> (Config, bool) {
        match toml::from_str(toml_str) {
            Ok(cfg) => (cfg, false),
            Err(_) => (Config::default(), true),
        }
    }

    /// Fold env-var overrides into the config so use sites never re-check env.
    /// `HYDRA_WORKTREE_ROOT` (non-empty) overrides the worktree root; `HYDRA_ALERTS=0`
    /// disables alerts. Pure — the actual env reads happen in `load`.
    pub fn with_env_overrides(
        mut self,
        worktree_root_env: Option<&str>,
        alerts_env: Option<&str>,
    ) -> Config {
        if let Some(root) = worktree_root_env {
            if !root.is_empty() {
                self.agent.worktree_root = root.to_string();
            }
        }
        if alerts_env == Some("0") {
            self.alerts.enabled = false;
        }
        self
    }

    /// Interpret the configured spawn mode. Unrecognized values degrade to `Window`
    /// (a typo must never silently change behavior), matching `parse_color`'s policy.
    pub fn spawn_mode(&self) -> SpawnMode {
        match self.agent.spawn_mode.trim().to_ascii_lowercase().as_str() {
            "session" => SpawnMode::Session,
            _ => SpawnMode::Window,
        }
    }
}

/// Parse a color string into a ratatui `Color`. Accepts named colors ("green",
/// "darkgray", …) and `#rrggbb` hex. Anything unrecognized returns `fallback`, so a
/// typo in the config degrades gracefully rather than crashing.
pub fn parse_color(s: &str, fallback: Color) -> Color {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(n) = u32::from_str_radix(hex, 16) {
                return Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8);
            }
        }
        return fallback;
    }
    match s.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        _ => fallback,
    }
}

/// Resolve the config file path: `$HYDRA_CONFIG`, else `$XDG_CONFIG_HOME/hydra/…`, else
/// `~/.config/hydra/config.toml`. Deliberately NOT `dirs::config_dir()` (that maps to
/// `~/Library/Application Support` on macOS; we want XDG-style `~/.config`).
pub fn default_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HYDRA_CONFIG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("hydra").join("config.toml"));
        }
    }
    dirs::home_dir().map(|h| h.join(".config").join("hydra").join("config.toml"))
}

/// Load the effective config, also returning a human notice when the config file
/// existed but could not be parsed (so the TUI can surface it). `None` when there was no
/// file or it parsed cleanly. Env overrides are folded in either way.
pub fn load_reporting() -> (Config, Option<String>) {
    let path = default_config_path();
    let (cfg, notice) = match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(contents) => {
            let (cfg, failed) = Config::parse_reporting(&contents);
            let notice = failed.then(|| {
                format!(
                    "config at {} couldn't be parsed — using defaults",
                    path.as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            });
            (cfg, notice)
        }
        None => (Config::default(), None),
    };
    let root = std::env::var("HYDRA_WORKTREE_ROOT").ok();
    let alerts = std::env::var("HYDRA_ALERTS").ok();
    (
        cfg.with_env_overrides(root.as_deref(), alerts.as_deref()),
        notice,
    )
}

/// Load the effective config (defaults if missing/unreadable/unparseable), env folded in.
pub fn load() -> Config {
    load_reporting().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_all_defaults() {
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn partial_config_fills_missing_fields_from_defaults() {
        // Only one field of one section set; everything else must be the default.
        let cfg = Config::parse("[timings]\nstale_after_secs = 42\n");
        assert_eq!(cfg.timings.stale_after_secs, 42);
        assert_eq!(cfg.timings.refresh_ms, 250); // default preserved
        assert_eq!(cfg.agent.command, "claude"); // absent section is default
    }

    #[test]
    fn full_config_round_trips_expected_values() {
        let toml = r#"
            [agent]
            command = "codex"
            worktree_root = "/tmp/wt"

            [popup]
            key = "h"
            width = "80%"
            height = "50%"

            [theme.tui]
            working = "cyan"

            [alerts]
            enabled = false
        "#;
        let cfg = Config::parse(toml);
        assert_eq!(cfg.agent.command, "codex");
        assert_eq!(cfg.agent.worktree_root, "/tmp/wt");
        assert_eq!(cfg.popup.key, "h");
        assert_eq!(cfg.popup.width, "80%");
        assert_eq!(cfg.theme.tui.working, "cyan");
        assert_eq!(cfg.theme.tui.idle, "gray"); // untouched default
        assert!(!cfg.alerts.enabled);
    }

    #[test]
    fn footer_colors_default_and_can_be_overridden() {
        // Defaults match the shortcut colors shown in the README screenshot.
        let d = ThemeTui::default();
        assert_eq!(d.footer_key, "green");
        assert_eq!(d.footer_label, "gray");

        let cfg = Config::parse("[theme.tui]\nfooter_key = \"#e6c384\"\n");
        assert_eq!(cfg.theme.tui.footer_key, "#e6c384");
        assert_eq!(cfg.theme.tui.footer_label, "gray"); // untouched default
    }

    #[test]
    fn row_theme_keys_default_to_the_old_hardcoded_colors() {
        let d = ThemeTui::default();
        assert_eq!(d.header, "blue");
        assert_eq!(d.branch, "cyan");
        assert_eq!(d.dirty, "magenta");
        assert_eq!(d.worktree_row, "darkgray");

        let cfg = Config::parse("[theme.tui]\nheader = \"#e6c384\"\n");
        assert_eq!(cfg.theme.tui.header, "#e6c384");
        assert_eq!(cfg.theme.tui.branch, "cyan"); // untouched default
    }

    #[test]
    fn syntax_error_falls_back_to_defaults() {
        assert_eq!(Config::parse("this is not = = toml"), Config::default());
    }

    #[test]
    fn parse_reporting_flags_unparseable_but_not_empty_or_valid() {
        assert_eq!(Config::parse_reporting(""), (Config::default(), false));
        let (cfg, failed) = Config::parse_reporting("[agent]\ncommand = \"x\"\n");
        assert_eq!(cfg.agent.command, "x");
        assert!(!failed);
        let (cfg, failed) = Config::parse_reporting("this = = not toml");
        assert_eq!(cfg, Config::default());
        assert!(failed);
    }

    #[test]
    fn parse_color_handles_names_hex_and_invalid() {
        assert_eq!(parse_color("green", Color::White), Color::Green);
        assert_eq!(parse_color("DarkGray", Color::White), Color::DarkGray);
        assert_eq!(parse_color("#32323c", Color::White), Color::Rgb(50, 50, 60));
        // Invalid hex length and unknown name fall back.
        assert_eq!(parse_color("#fff", Color::White), Color::White);
        assert_eq!(parse_color("chartreuse", Color::Black), Color::Black);
    }

    #[test]
    fn env_overrides_win_over_file_values() {
        let base = Config::parse("[agent]\nworktree_root = \"/from/file\"\n");
        // HYDRA_WORKTREE_ROOT set and non-empty overrides; HYDRA_ALERTS=0 disables.
        let merged = base
            .clone()
            .with_env_overrides(Some("/from/env"), Some("0"));
        assert_eq!(merged.agent.worktree_root, "/from/env");
        assert!(!merged.alerts.enabled);

        // Empty env root is ignored; missing alerts env leaves default (enabled).
        let unchanged = base.with_env_overrides(Some(""), None);
        assert_eq!(unchanged.agent.worktree_root, "/from/file");
        assert!(unchanged.alerts.enabled);
    }

    #[test]
    fn spawn_mode_defaults_to_window() {
        assert_eq!(Config::default().spawn_mode(), SpawnMode::Window);
        assert_eq!(Config::parse("").spawn_mode(), SpawnMode::Window);
    }

    #[test]
    fn spawn_mode_parses_session_case_insensitively() {
        assert_eq!(
            Config::parse("[agent]\nspawn_mode = \"session\"\n").spawn_mode(),
            SpawnMode::Session
        );
        assert_eq!(
            Config::parse("[agent]\nspawn_mode = \"Session\"\n").spawn_mode(),
            SpawnMode::Session
        );
    }

    #[test]
    fn spawn_mode_unknown_value_degrades_to_window_without_breaking_config() {
        // A typo must not fall the whole config back to defaults, just the mode.
        let cfg = Config::parse("[agent]\nspawn_mode = \"bogus\"\ncommand = \"codex\"\n");
        assert_eq!(cfg.spawn_mode(), SpawnMode::Window);
        assert_eq!(cfg.agent.command, "codex");
    }
}
