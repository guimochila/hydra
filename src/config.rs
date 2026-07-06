//! The on-disk config contract. Mirrors how `state.rs` owns the state contract:
//! this module is the single source of truth for user-tunable settings. Every field
//! has a `Default` matching Hydra's built-in constant, so a missing or partial config
//! yields today's exact behavior. Loading (IO) is thin; parsing/merge is pure.
//!
//! (`ratatui::style::Color` and `std::path::PathBuf` are imported in the tasks that
//! first use them — Task 2 and Task 3 respectively.)

use serde::Deserialize;

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
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            command: "claude".to_string(),
            worktree_root: "~/work/tree".to_string(),
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
}

impl Default for ThemeTui {
    fn default() -> Self {
        Self {
            highlight_bg: "#32323c".to_string(), // Rgb(50,50,60)
            working: "green".to_string(),
            needs_input: "yellow".to_string(),
            idle: "gray".to_string(),
            unknown: "darkgray".to_string(),
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

impl Config {
    /// Parse TOML into a `Config`, filling missing fields from defaults. On a syntax or
    /// type error, fall back to all defaults (the config must never break Hydra).
    pub fn parse(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap_or_default()
    }
}

// parse_color, env overrides, and load() are added in later tasks.

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
    fn syntax_error_falls_back_to_defaults() {
        assert_eq!(Config::parse("this is not = = toml"), Config::default());
    }
}
