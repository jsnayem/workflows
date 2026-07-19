//! Theme + verbosity system for the wf TUI.
//!
//! Colors are never referenced as raw `Color::*` inside the draw code — every
//! panel styles itself from a `Theme`, which maps *semantic roles* to colors.
//! That keeps the four tabs visually consistent and lets the user swap palettes
//! live (`t`) without touching any draw logic.
//!
//! Settings persist to an XDG config file (`~/.config/wf/config.toml`)
//! so the chosen theme + verbosity survive restarts.

use ratatui::style::{Color, Modifier, Style};
use std::path::PathBuf;

/// Semantic color roles. A panel asks for "heading", not "cyan".
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub heading: Color, // section titles, tab labels, table headers
    pub label: Color,   // field / column names
    pub value: Color,   // normal data
    pub good: Color,    // running / clean / healthy
    pub warn: Color,    // dirty / ahead-behind / caution
    pub bad: Color,     // stopped / errors / secrets
    pub accent: Color,  // active tab, selection highlight
    pub muted: Color,   // units, deltas, timestamps, footer hints
    pub border: Color,  // block borders
    pub bold: bool,     // apply Modifier::BOLD to headings + accents
}

impl Theme {
    /// All built-in themes, in cycle order.
    pub const ALL: &'static [Theme] = &[DARK, NORD, HIGH_CONTRAST, MONO];

    /// Resolve a theme by (case-insensitive) name; falls back to DARK.
    pub fn resolve(name: &str) -> Theme {
        let n = name.to_ascii_lowercase();
        for t in Self::ALL {
            if t.name == n {
                return *t;
            }
        }
        DARK
    }

    /// The next theme in the cycle (wraps).
    pub fn next(&self) -> Theme {
        let mut iter = Self::ALL.iter();
        while let Some(t) = iter.next() {
            if t.name == self.name {
                break;
            }
        }
        iter.next().copied().unwrap_or(DARK)
    }

    fn styled(&self, c: Color, bold: bool) -> Style {
        let mut s = Style::default().fg(c);
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    }

    pub fn heading(&self) -> Style {
        self.styled(self.heading, self.bold)
    }
    pub fn label(&self) -> Style {
        self.styled(self.label, false)
    }
    pub fn value(&self) -> Style {
        self.styled(self.value, false)
    }
    pub fn good(&self) -> Style {
        self.styled(self.good, self.bold)
    }
    pub fn warn(&self) -> Style {
        self.styled(self.warn, self.bold)
    }
    pub fn bad(&self) -> Style {
        self.styled(self.bad, self.bold)
    }
    pub fn accent(&self) -> Style {
        self.styled(self.accent, self.bold)
    }
    pub fn muted(&self) -> Style {
        self.styled(self.muted, false)
    }
    pub fn border(&self) -> Style {
        self.styled(self.border, false)
    }
    /// Active-tab / selection highlight (reversed accent).
    pub fn selected(&self) -> Style {
        Style::default()
            .bg(self.accent)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    }
}

// --- palettes ---

pub const DARK: Theme = Theme {
    name: "dark",
    heading: Color::Cyan,
    label: Color::Blue,
    value: Color::Gray,
    good: Color::Green,
    warn: Color::Yellow,
    bad: Color::Red,
    accent: Color::Magenta,
    muted: Color::DarkGray,
    border: Color::Cyan,
    bold: false,
};

pub const NORD: Theme = Theme {
    name: "nord",
    heading: Color::Rgb(129, 161, 193), // nord8 frost blue
    label: Color::Rgb(136, 192, 208),   // nord7 ice
    value: Color::Rgb(216, 222, 233),   // nord4 snow storm
    good: Color::Rgb(163, 190, 140),    // nord14
    warn: Color::Rgb(235, 203, 139),    // nord13
    bad: Color::Rgb(191, 97, 106),      // nord11
    accent: Color::Rgb(180, 142, 173),  // nord15
    muted: Color::Rgb(94, 109, 131),    // nord3
    border: Color::Rgb(76, 86, 106),    // nord3-alt
    bold: false,
};

pub const HIGH_CONTRAST: Theme = Theme {
    name: "high-contrast",
    heading: Color::LightCyan,
    label: Color::LightBlue,
    value: Color::White,
    good: Color::LightGreen,
    warn: Color::LightYellow,
    bad: Color::LightRed,
    accent: Color::LightMagenta,
    muted: Color::DarkGray,
    border: Color::White,
    bold: true,
};

pub const MONO: Theme = Theme {
    name: "mono",
    heading: Color::Reset,
    label: Color::Reset,
    value: Color::Reset,
    good: Color::Reset,
    warn: Color::Reset,
    bad: Color::Reset,
    accent: Color::Reset,
    muted: Color::Reset,
    border: Color::Reset,
    bold: false,
};

/// Persisted UI settings.
#[derive(Debug, Clone)]
pub struct Config {
    pub theme: Theme,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: DARK,
            verbose: false,
        }
    }
}

impl Config {
    /// XDG path: ~/.config/wf/config.toml (falls back to ~/.wf.toml).
    fn path() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME") {
            let xdg = PathBuf::from(&home).join(".config/wf/config.toml");
            if let Some(parent) = xdg.parent() {
                if parent.exists() {
                    return xdg;
                }
            }
            return PathBuf::from(&home).join(".wf.toml");
        }
        PathBuf::from(".wf.toml")
    }

    /// Load from disk; any missing/corrupt field falls back to defaults.
    pub fn load() -> Config {
        let p = Self::path();
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Config::default();
        };
        let Ok(tbl) = text.parse::<toml::Value>() else {
            return Config::default();
        };
        let theme_name = tbl.get("theme").and_then(|v| v.as_str()).unwrap_or("dark");
        let verbose = tbl
            .get("verbose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Config {
            theme: Theme::resolve(theme_name),
            verbose,
        }
    }

    /// Persist to disk (best-effort; ignore write errors).
    pub fn save(&self) {
        let p = Self::path();
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = format!(
            "# wf TUI settings\n# theme: dark | nord | high-contrast | mono\ntheme = \"{}\"\n# verbose: explain technical headings (per-section captions)\nverbose = {}\n",
            self.theme.name, self.verbose
        );
        let _ = std::fs::write(&p, body);
    }
}
