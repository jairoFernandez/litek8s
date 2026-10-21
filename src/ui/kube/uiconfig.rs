//! `~/.litek8s/ui.yaml` (next to config.yaml; `$LITEK8S_UI_CONFIG`): the
//! Kubernetes view's aliases, hotkeys, plugins, skin and log settings.
//!
//! ```yaml
//! aliases:
//!   cp: pods -n kube-system -l tier=control-plane
//! hotkeys:
//!   shift-1: ":pods -A"
//!   f2: ":deploy"
//! plugins:
//!   - name: dive
//!     key: ctrl-i
//!     scopes: [pods]
//!     command: dive
//!     args: [$COL-IMAGE]
//! writableContexts: [staging]   # other clusters are read-only
//! showOtherContexts: true        # false: Clusters lists litek8s's only
//! theme: default          # high-contrast, mono
//! background: auto        # dark, light
//! ascii: false            # or LITEK8S_UI_ASCII=1
//! skin: {accent: cyan, selected: darkgray}
//! logs: {tail: 200, buffer: 20000, timestamps: false}
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub aliases: BTreeMap<String, String>,
    /// Key → command line (`:pods -A`, `!get events`).
    pub hotkeys: BTreeMap<String, String>,
    pub plugins: Vec<Plugin>,
    pub skin: Skin,
    pub logs: LogsConfig,
    /// No deletes, edits, scaling or shells, on any cluster.
    #[serde(rename = "readOnly", alias = "read_only")]
    pub read_only: bool,
    /// Contexts that are not litek8s clusters but may be changed from the
    /// UI (others are read-only).
    #[serde(rename = "writableContexts", alias = "writable_contexts")]
    pub writable_contexts: Vec<String>,
    /// List the kubeconfig's other contexts in the Clusters view.
    #[serde(rename = "showOtherContexts", alias = "show_other_contexts")]
    pub show_other_contexts: bool,
    /// kubectl to run for `:kubectl` (default: `kubectl` on the PATH).
    pub kubectl: Option<String>,
    /// The whole UI's look: `default`, `high-contrast`, `mono`.
    pub theme: crate::ui::theme::Preset,
    /// `auto` (ask the terminal), `dark`, `light`.
    pub background: crate::ui::theme::Background,
    /// ASCII instead of box-drawing and block characters (also
    /// `LITEK8S_UI_ASCII=1`).
    pub ascii: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            aliases: BTreeMap::new(),
            hotkeys: BTreeMap::new(),
            plugins: Vec::new(),
            skin: Skin::default(),
            logs: LogsConfig::default(),
            read_only: false,
            writable_contexts: Vec::new(),
            show_other_contexts: true,
            kubectl: None,
            theme: Default::default(),
            background: Default::default(),
            ascii: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Plugin {
    pub name: String,
    pub key: String,
    /// Resource names (`pods`, `deploy`, `all`).
    pub scopes: Vec<String>,
    pub description: String,
    pub command: String,
    /// `$NAME $NAMESPACE $CONTEXT $RESOURCE $CONTAINER $COL-<COLUMN>` are
    /// replaced (also given as environment variables).
    pub args: Vec<String>,
    /// Run without leaving the UI; the output shows in a pager.
    pub background: bool,
    /// Ask first.
    pub confirm: bool,
    /// It may change the cluster (the default): not run on read-only
    /// contexts. `false` for ones that only read.
    pub mutates: bool,
}

impl Default for Plugin {
    fn default() -> Self {
        Plugin { name: String::new(), key: String::new(), scopes: vec!["all".into()], description: String::new(), command: String::new(), args: Vec::new(), background: false, confirm: false, mutates: true }
    }
}

impl Plugin {
    pub fn applies(&self, resource_names: &[&str]) -> bool {
        self.scopes.iter().any(|s| s == "all" || s == "*" || resource_names.iter().any(|n| n.eq_ignore_ascii_case(s)))
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    /// Lines of each container's past log to start with.
    pub tail: u32,
    /// Lines kept in memory (oldest dropped).
    pub buffer: usize,
    pub timestamps: bool,
    pub wrap: bool,
    /// At most this many streams for a workload's logs.
    #[serde(rename = "maxStreams", alias = "max_streams")]
    pub max_streams: usize,
}

impl Default for LogsConfig {
    fn default() -> Self {
        LogsConfig { tail: 100, buffer: 10_000, timestamps: false, wrap: false, max_streams: 30 }
    }
}

/// Colors by name (`cyan`, `lightred`, `darkgray`), number (0-255) or
/// `#rrggbb`, over the theme's; empty (the default) keeps the theme's.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Skin {
    pub accent: String,
    pub ok: String,
    pub warn: String,
    pub bad: String,
    pub done: String,
    pub dim: String,
    pub selected: String,
    pub marked: String,
    pub key: String,
}

impl Default for Skin {
    fn default() -> Self {
        Skin {
            accent: String::new(),
            ok: String::new(),
            warn: String::new(),
            bad: String::new(),
            done: String::new(),
            dim: String::new(),
            selected: String::new(),
            marked: String::new(),
            key: String::new(),
        }
    }
}

pub fn color(s: &str) -> Option<Color> {
    let s = s.trim().to_lowercase();
    Some(match s.as_str() {
        "" | "default" | "reset" | "none" => return None,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ if s.starts_with('#') && s.len() == 7 => {
            let n = u32::from_str_radix(&s[1..], 16).ok()?;
            Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
        }
        _ => Color::Indexed(s.parse().ok()?),
    })
}

pub use crate::ui::theme::Theme;

/// `ctrl-x`, `shift-a`, `alt-1`, `f5`, `x`, `X`, `enter`, `tab`.
pub fn parse_key(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let lower = s.trim().to_lowercase();
    let mut mods = KeyModifiers::NONE;
    let mut rest = lower.as_str();
    loop {
        if let Some(r) = rest.strip_prefix("ctrl-").or_else(|| rest.strip_prefix("ctrl+")) {
            mods |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("alt-").or_else(|| rest.strip_prefix("alt+")) {
            mods |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("shift-").or_else(|| rest.strip_prefix("shift+")) {
            mods |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }
    let code = match rest {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "delete" | "del" => KeyCode::Delete,
        _ if rest.len() > 1 && rest.starts_with('f') => KeyCode::F(rest[1..].parse().ok()?),
        _ if rest.chars().count() == 1 => {
            // The original case: `X` is shift-x.
            let orig = s.trim().chars().last()?;
            let c = if mods.contains(KeyModifiers::SHIFT) { shifted(orig.to_ascii_lowercase()) } else { orig };
            if c.is_ascii_uppercase() || !c.is_ascii_alphanumeric() {
                mods.remove(KeyModifiers::SHIFT);
            }
            KeyCode::Char(c)
        }
        _ => return None,
    };
    Some((code, mods))
}

/// What shift gives on a US keyboard (`shift-1` is `!`).
fn shifted(c: char) -> char {
    match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        _ => c,
    }
}

pub fn key_matches(spec: &str, k: &KeyEvent) -> bool {
    let Some((code, mods)) = parse_key(spec) else { return false };
    let kmods = k.modifiers - KeyModifiers::SHIFT;
    let want = mods - KeyModifiers::SHIFT;
    let code_eq = match (code, k.code) {
        (KeyCode::Char(a), KeyCode::Char(b)) if want.contains(KeyModifiers::CONTROL) => a.eq_ignore_ascii_case(&b),
        (a, b) => a == b,
    };
    code_eq && kmods == want
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("LITEK8S_UI_CONFIG") {
        return PathBuf::from(p);
    }
    crate::dev::config::path().with_file_name("ui.yaml")
}

pub fn history_path() -> PathBuf {
    crate::dev::config::path().with_file_name("ui-history")
}

/// The file's settings; defaults when there is none; an error says what is
/// wrong in it.
pub fn load() -> Result<UiConfig, String> {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(text) if text.trim().is_empty() => Ok(UiConfig::default()),
        Ok(text) => serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UiConfig::default()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_file_and_keys() {
        let c: UiConfig = serde_yaml::from_str(
            "aliases: {cp: pods -n kube-system}\nhotkeys: {shift-1: ':pods -A', f2: ':deploy'}\n\
             plugins:\n- {name: dive, key: ctrl-i, scopes: [pods], command: dive, args: [$COL-IMAGE]}\n\
             skin: {accent: '#ff8800', selected: '238'}\nlogs: {tail: 5}\nreadOnly: true\n",
        )
        .unwrap();
        assert_eq!(c.aliases["cp"], "pods -n kube-system");
        assert_eq!(c.logs.tail, 5);
        assert_eq!(c.logs.buffer, 10_000);
        assert!(c.read_only);
        assert!(c.plugins[0].applies(&["pods", "po"]) && !c.plugins[0].applies(&["deploy"]));
        assert_eq!(color(&c.skin.accent), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(color(&c.skin.selected), Some(Color::Indexed(238)));
        assert!(serde_yaml::from_str::<UiConfig>("nope: 1").is_err());

        let k = |c: char, m: KeyModifiers| KeyEvent::new(KeyCode::Char(c), m);
        assert!(key_matches("ctrl-i", &k('i', KeyModifiers::CONTROL)));
        assert!(key_matches("shift-1", &k('!', KeyModifiers::SHIFT)));
        assert!(key_matches("shift-a", &k('A', KeyModifiers::SHIFT)));
        assert!(key_matches("A", &k('A', KeyModifiers::NONE)));
        assert!(!key_matches("a", &k('A', KeyModifiers::SHIFT)));
        assert!(key_matches("f2", &KeyEvent::from(KeyCode::F(2))));
        assert!(key_matches("alt-x", &k('x', KeyModifiers::ALT)));
        assert!(!key_matches("x", &k('x', KeyModifiers::ALT)));
    }
}
