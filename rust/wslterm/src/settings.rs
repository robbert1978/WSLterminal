//! User settings, loaded from `%APPDATA%\WslTerminal\settings.json` — the same
//! file and schema the C# app uses (colors are "#RRGGBB"; FontSize in points;
//! Opacity a 10..100 percent). Rust-only background-image keys are optional, so
//! old settings files keep their existing behavior.

use serde::Deserialize;
use wslterm_core::color;

use crate::background::{BackgroundConfig, BackgroundFit};

/// Resolved appearance.
#[derive(Clone)]
pub struct Settings {
    pub font_family: String,
    pub font_pts: f32,
    pub opacity: f32, // 0.0..=1.0
    pub editor: String, // command run (in a new terminal tab) to open files
    pub background: BackgroundConfig,
    pub theme: Theme,
    pub actions: Vec<ActionBinding>, // user keybindings (settings.json "actions")
}

/// One user keybinding from settings.json's `actions` array (Windows Terminal
/// shape): a `keys` chord (e.g. `"ctrl+shift+up"`) bound to a command. `action`
/// is the command name (e.g. `"sendInput"`); `input` is its payload, already
/// JSON-unescaped (so `"\u001b[1;6A"` arrives as the real ESC sequence). The host
/// (`main.rs`) compiles these into matchable bindings.
#[derive(Clone)]
pub struct ActionBinding {
    pub keys: String,
    pub action: String,
    pub input: String,
}

/// Concrete RGB palette used by the renderer.
#[derive(Clone)]
pub struct Theme {
    pub bg: u32,
    pub fg: u32,
    pub cursor: u32,
    pub selection: u32,
    pub ansi: [u32; 16],
}

impl Theme {
    /// Resolve a cell color code (see `wslterm_core::color`) to concrete RGB.
    /// `default_rgb` is this theme's fg or bg, chosen by the caller. `bold`
    /// brightens the low 8 ANSI colors.
    pub fn resolve(&self, code: i32, default_rgb: u32, bold: bool) -> u32 {
        if code == color::DEFAULT {
            return default_rgb;
        }
        if code & color::TRUE_COLOR != 0 {
            return (code & 0xFF_FFFF) as u32;
        }
        let mut idx = (code & 0xFF) as usize;
        if bold && idx < 8 {
            idx += 8;
        }
        if idx < 16 {
            self.ansi[idx]
        } else {
            color::palette(idx) // 216-cube + grays are scheme-independent
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct Raw {
    font_family: String,
    font_size: f32,
    background: String,
    foreground: String,
    cursor: String,
    selection: String,
    opacity: u32,
    editor: String,
    background_image: Option<String>,
    background_image_opacity: u32,
    background_image_fit: String,
    ansi: Vec<String>,
    // Keybindings. Lowercase `actions` (Windows Terminal style); `Actions` also ok.
    #[serde(rename = "actions", alias = "Actions")]
    actions: Vec<RawAction>,
}

#[derive(Deserialize, Clone)]
struct RawAction {
    #[serde(default)]
    keys: String,
    command: RawCommand,
}

/// A `command` is either a bare action name (`"command": "copy"`) or an object
/// with parameters (`"command": { "action": "sendInput", "input": "..." }`).
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum RawCommand {
    Named(String),
    Detailed {
        action: String,
        #[serde(default)]
        input: String,
    },
}

impl Default for Raw {
    fn default() -> Self {
        Raw {
            font_family: "Cascadia Mono".into(),
            font_size: 12.0,
            background: "#0C0C0C".into(),
            foreground: "#CCCCCC".into(),
            cursor: "#FFFFFF".into(),
            selection: "#264F78".into(),
            opacity: 100,
            editor: "nano".into(),
            background_image: None,
            background_image_opacity: 35,
            background_image_fit: "cover".into(),
            ansi: CAMPBELL.iter().map(|s| s.to_string()).collect(),
            actions: Vec::new(),
        }
    }
}

const CAMPBELL: [&str; 16] = [
    "#0C0C0C", "#C50F1F", "#13A10E", "#C19C00", "#0037DA", "#881798", "#3A96DD", "#CCCCCC",
    "#767676", "#E74856", "#16C60C", "#F9F1A5", "#3B78FF", "#B4009E", "#61D6D6", "#F2F2F2",
];

impl Settings {
    /// `%APPDATA%\WslTerminal\settings.json`.
    pub fn path() -> Option<std::path::PathBuf> {
        let appdata = std::env::var("APPDATA").ok()?;
        Some(std::path::PathBuf::from(appdata).join("WslTerminal").join("settings.json"))
    }

    /// Load settings, falling back to defaults on any error.
    pub fn load() -> Settings {
        let raw = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<Raw>(&s).ok())
            .unwrap_or_default();
        Settings::from(raw)
    }
}

impl From<Raw> for Settings {
    fn from(r: Raw) -> Self {
        let hex = |s: &str, fb: u32| color::parse_hex(s, fb);
        let mut ansi = [0u32; 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            let fb = color::parse_hex(CAMPBELL[i], 0);
            *slot = r.ansi.get(i).map(|s| hex(s, fb)).unwrap_or(fb);
        }
        let actions = r
            .actions
            .into_iter()
            .map(|ra| {
                let (action, input) = match ra.command {
                    RawCommand::Named(s) => (s, String::new()),
                    RawCommand::Detailed { action, input } => (action, input),
                };
                ActionBinding { keys: ra.keys, action, input }
            })
            .collect();
        Settings {
            font_family: r.font_family,
            font_pts: if r.font_size > 0.0 { r.font_size } else { 12.0 },
            opacity: (r.opacity.clamp(10, 100) as f32) / 100.0,
            editor: if r.editor.trim().is_empty() { "nano".into() } else { r.editor },
            background: BackgroundConfig {
                path: r
                    .background_image
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from),
                opacity: (r.background_image_opacity.min(100) as f32) / 100.0,
                fit: BackgroundFit::parse(&r.background_image_fit),
            },
            theme: Theme {
                bg: hex(&r.background, 0x0C_0C0C),
                fg: hex(&r.foreground, 0xCC_CCCC),
                cursor: hex(&r.cursor, 0xFF_FFFF),
                selection: hex(&r.selection, 0x26_4F78),
                ansi,
            },
            actions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions_object_and_string_forms() {
        // Object form (action + input) and the bare-string command form.
        let json = r#"{
          "FontFamily": "Cascadia Mono",
          "actions": [
            { "keys": "ctrl+shift+up", "command": { "action": "sendInput", "input": "hi" } },
            { "keys": "ctrl+shift+c",  "command": "copy" }
          ]
        }"#;
        let s = Settings::from(serde_json::from_str::<Raw>(json).unwrap());
        assert_eq!(s.actions.len(), 2);
        assert_eq!(s.actions[0].keys, "ctrl+shift+up");
        assert_eq!(s.actions[0].action, "sendInput");
        assert_eq!(s.actions[0].input, "hi");
        assert_eq!(s.actions[1].action, "copy"); // bare string -> empty input
        assert_eq!(s.actions[1].input, "");
    }

    #[test]
    fn missing_actions_defaults_empty() {
        let s = Settings::from(serde_json::from_str::<Raw>(r#"{"FontFamily":"x"}"#).unwrap());
        assert!(s.actions.is_empty());
    }
}
