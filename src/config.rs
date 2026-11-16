//! Configuration: TOML defaults merged with the user's file, then shipped to
//! the in-page UI layer as JSON. One source of truth for keys, search engines
//! and engine flags.

use serde_json::{json, Map, Value};

pub const DEFAULT_CONFIG: &str = r#"
# kite — keyboard-first Chromium shell
[general]
engine = "chromium"
start_page = "about:blank"
search_engine = "ddg"
width = 1280
height = 820
# Hide the engine's window frame. On Wayland this disables Chromium's own
# title bar; on X11 kite asks the window manager to drop the frame.
# Set it to true if you want a normal decorated window.
decorations = false
# Start fullscreen. The one way to guarantee no frame on any compositor.
fullscreen = false
# auto: bar appears on load/command/hint, hides otherwise (best for small screens)
bar = "auto"
status = true
hints_chars = "asdfghjkl;"
scroll_step = 84
smooth_scroll = false
images = true
javascript = true
zoom = 1.0
restore_session = true
extra_engine_args = []

[search]
ddg = "https://duckduckgo.com/?q={}"
startpage = "https://www.startpage.com/sp/search?query={}"
wikipedia = "https://en.wikipedia.org/w/index.php?search={}"
github = "https://github.com/search?q={}"
youtube = "https://www.youtube.com/results?search_query={}"
arch = "https://archlinux.org/packages/?q={}"

[keys.normal]
"j" = "scroll down"
"k" = "scroll up"
"h" = "scroll left"
"l" = "scroll right"
"<Down>" = "scroll down"
"<Up>" = "scroll up"
"<Space>" = "scroll page-down"
"<BS>" = "scroll page-up"
"<C-d>" = "scroll half-page-down"
"<C-u>" = "scroll half-page-up"
"<C-f>" = "scroll page-down"
"<C-b>" = "scroll page-up"
"gg" = "scroll top"
"G" = "scroll bottom"
"$" = "scroll right-edge"
"0" = "scroll left-edge"

"H" = "back"
"L" = "forward"
"r" = "reload"
"R" = "reload-hard"
"<C-r>" = "reload-hard"
"s" = "stop"
"gh" = "home"

"o" = "open"
"O" = "open-newtab"
"go" = "open-edit"
":" = "command"
"/" = "find"
"?" = "help"

"f" = "hint"
"F" = "hint-newtab"
";y" = "hint-yank"
";Y" = "hint-yank-newtab"

"n" = "find-next"
"N" = "find-prev"
"<Esc>" = "clear"

"t" = "tab-new"
"d" = "tab-close"
"x" = "tab-close"
"gt" = "tab-next"
"gT" = "tab-prev"
"<C-Tab>" = "tab-next"
"<C-S-Tab>" = "tab-prev"
"<C-w>" = "tab-close"
"<C-t>" = "tab-new"
"u" = "tab-undo-close"
"g1" = "tab-first"
"g9" = "tab-last"

"yy" = "yank-url"
"yt" = "yank-title"
"p" = "paste-open"
"P" = "paste-open-newtab"
"m" = "bookmark-add"
"gb" = "bookmark-list"
"<C-h>" = "history-list"

"+" = "zoom-in"
"=" = "zoom-in"
"-" = "zoom-out"
"<C-0>" = "zoom-reset"

"i" = "mode insert"
"gi" = "focus-input"
"v" = "mode passthrough"
"<C-o>" = "external"

"ZQ" = "quit"
"ZZ" = "quit"
"<C-q>" = "quit"

[keys.insert]
"<Esc>" = "mode normal"
"<C-Esc>" = "mode normal"

[keys.passthrough]
"<Esc>" = "mode normal"
"<C-Esc>" = "mode normal"

[keys.hint]
"<Esc>" = "hint-cancel"
"<CR>" = "hint-enter"

[keys.command]
"<Esc>" = "command-cancel"
"<CR>" = "command-enter"
"<C-w>" = "command-word-delete"
"<C-u>" = "command-clear"
"<C-n>" = "command-next"
"<C-p>" = "command-prev"
"<Tab>" = "command-next"
"<C-j>" = "command-next"
"<C-k>" = "command-prev"
"<BS>" = "command-backspace"
"<C-h>" = "command-backspace"
"<C-v>" = "command-paste"
"<Left>" = "command-left"
"<Right>" = "command-right"
"<C-a>" = "command-home"
"<C-e>" = "command-end"
"<A-BS>" = "command-word-delete"
"<C-l>" = "command-clear"
"<Up>" = "command-prev"
"<Down>" = "command-next"
"<TS>" = "command-next"
"<C-CR>" = "command-enter-tab"
"<S-CR>" = "command-enter-tab"
"#;

#[derive(Clone, Debug)]
pub struct Config {
    pub general: Map<String, Value>,
    pub search: Map<String, Value>,
    pub keys: Map<String, Value>,
    pub raw: Value,
    pub path: Option<std::path::PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_toml(DEFAULT_CONFIG).expect("built-in config must parse")
    }
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let parsed: toml::Value = text
            .parse::<toml::Value>()
            .map_err(|e| format!("config: {e}"))?;
        let as_json: Value = serde_json::to_value(parsed).map_err(|e| e.to_string())?;
        let obj = as_json.as_object().cloned().unwrap_or_default();
        Ok(Config {
            general: obj
                .get("general")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default(),
            search: obj
                .get("search")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default(),
            keys: obj
                .get("keys")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default(),
            raw: as_json,
            path: None,
        })
    }

    /// Load defaults, then the system config (NixOS module, /etc/xdg), then the
    /// user's file on top — standard XDG precedence.
    pub fn load(
        system: Option<&std::path::Path>,
        user: &std::path::Path,
    ) -> Result<Config, String> {
        let mut raw = Config::from_toml(DEFAULT_CONFIG)?.raw;
        if let Some(sys) = system {
            if sys.exists() {
                let text =
                    std::fs::read_to_string(sys).map_err(|e| format!("{}: {e}", sys.display()))?;
                let v: toml::Value = text
                    .parse::<toml::Value>()
                    .map_err(|e| format!("{}: {e}", sys.display()))?;
                raw = deep_merge(raw, serde_json::to_value(v).map_err(|e| e.to_string())?);
            }
        }
        if user.exists() {
            let text =
                std::fs::read_to_string(user).map_err(|e| format!("{}: {e}", user.display()))?;
            let v: toml::Value = text
                .parse::<toml::Value>()
                .map_err(|e| format!("{}: {e}", user.display()))?;
            raw = deep_merge(raw, serde_json::to_value(v).map_err(|e| e.to_string())?);
        }
        let mut c = Config::from_toml(&toml::to_string(&raw).map_err(|e| e.to_string())?)?;
        c.path = Some(user.to_path_buf());
        Ok(c)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.general.get(key).and_then(|v| v.as_str())
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.general.get(key).and_then(|v| v.as_bool())
    }
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.general.get(key).and_then(|v| v.as_i64())
    }
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.general.get(key).and_then(|v| v.as_f64())
    }
    pub fn extra_args(&self) -> Vec<String> {
        self.general
            .get("extra_engine_args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Canonical keymap handed to the UI: `{"normal": {"g g": "scroll top"}}`.
    /// Keys are normalised to space-separated tokens so JS can match sequences.
    pub fn keymap_json(&self) -> Value {
        let mut out = Map::new();
        for (mode, table) in &self.keys {
            let mut m = Map::new();
            if let Some(tbl) = table.as_object() {
                for (k, v) in tbl {
                    let action = match v.as_str() {
                        Some(s) if !s.is_empty() => s,
                        _ => continue,
                    };
                    let toks = normalize_key_seq(k);
                    if toks.is_empty() {
                        continue;
                    }
                    m.insert(toks.join(" "), json!(action));
                }
            }
            out.insert(mode.clone(), Value::Object(m));
        }
        Value::Object(out)
    }

    /// Everything the UI layer needs at boot.
    pub fn ui_json(&self, extra: Value) -> Value {
        let mut base = json!({
            "keymap": self.keymap_json(),
            "search": Value::Object(self.search.clone()),
            "settings": {
                "bar": self.get_str("bar").unwrap_or("auto"),
                "status": self.get_bool("status").unwrap_or(true),
                "hints_chars": self.get_str("hints_chars").unwrap_or("asdfghjkl;"),
                "scroll_step": self.get_i64("scroll_step").unwrap_or(84),
                "smooth_scroll": self.get_bool("smooth_scroll").unwrap_or(false),
                "images": self.get_bool("images").unwrap_or(true),
                "javascript": self.get_bool("javascript").unwrap_or(true),
                "zoom": self.get_f64("zoom").unwrap_or(1.0),
                "search_engine": self.get_str("search_engine").unwrap_or("ddg"),
            },
        });
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e.clone() {
                b.insert(k, v);
            }
        }
        base
    }
}

fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut x), Value::Object(y)) => {
            for (k, v) in y {
                let merged = match x.remove(&k) {
                    Some(old) => deep_merge(old, v),
                    None => v,
                };
                x.insert(k, merged);
            }
            Value::Object(x)
        }
        (_, b) => b,
    }
}

/// "gg" -> ["g","g"], "<C-d>" -> ["<C-d>"], "y y" -> ["y","y"], "ZQ" -> ["Z","Q"]
pub fn normalize_key_seq(spec: &str) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '<' {
            if let Some(end) = chars[i..].iter().position(|&ch| ch == '>') {
                let inner: String = chars[i + 1..i + end].iter().collect();
                if !inner.is_empty() {
                    toks.push(format!("<{}>", canonical_angle(&inner)));
                    i += end + 1;
                    continue;
                }
            }
            toks.push("<".to_string());
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else {
            toks.push(c.to_string());
            i += 1;
        }
    }
    toks
}

fn canonical_angle(inner: &str) -> String {
    let mut prefix = String::new();
    let mut rest = inner;
    loop {
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("ctrl-") || lower.starts_with("c-") {
            prefix.push_str("C-");
            rest = &rest[rest.find('-').unwrap() + 1..];
        } else if lower.starts_with("alt-")
            || lower.starts_with("a-")
            || lower.starts_with("meta-")
            || lower.starts_with("m-")
        {
            prefix.push_str("A-");
            rest = &rest[rest.find('-').unwrap() + 1..];
        } else if lower.starts_with("shift-") || lower.starts_with("s-") {
            prefix.push_str("S-");
            rest = &rest[rest.find('-').unwrap() + 1..];
        } else {
            break;
        }
    }
    let body = match rest.to_ascii_lowercase().as_str() {
        "esc" | "escape" => "Esc".to_string(),
        "cr" | "enter" | "return" => "CR".to_string(),
        "space" => "Space".to_string(),
        "tab" => "Tab".to_string(),
        "bs" | "backspace" => "BS".to_string(),
        "del" | "delete" => "Del".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pgup" | "pageup" => "PgUp".to_string(),
        "pgdn" | "pagedown" => "PgDn".to_string(),
        "ts" => "TS".to_string(),
        _ => rest.to_string(),
    };
    format!("{prefix}{body}")
}

pub fn config_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("kite").join("config.toml")
}

/// System-wide config, as written by the NixOS module. First match wins.
pub fn system_config_path() -> Option<std::path::PathBuf> {
    let dirs = std::env::var("XDG_CONFIG_DIRS").unwrap_or_else(|_| "/etc/xdg".to_string());
    for dir in dirs.split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = std::path::Path::new(dir).join("kite").join("config.toml");
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn state_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state"));
    base.join("kite")
}

pub fn home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
}
