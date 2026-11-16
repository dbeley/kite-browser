//! The built-in start page: a static file written into the state directory.
//!
//! Rolled out as a file rather than a data: URL so the session, history and
//! status line keep a short, readable address, and so there is no server in
//! the path. The page is replaced on start when its contents change.

use std::path::{Path, PathBuf};

const HTML: &str = include_str!("home.html");

/// Default stack, used when the config says nothing: the first four are what
/// people install on purpose, the rest is what every machine has.
const FALLBACK_FONT: &str =
    "ui-monospace, \"Iosevka Term\", \"Overpass Mono\", \"JetBrains Mono\", \"DejaVu Sans Mono\", monospace";

/// Keep the font from breaking out of the stylesheet: a family list is letters,
/// digits, spaces, commas, dashes and quotes.
fn css_font(font: &str) -> String {
    font.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | ',' | '-' | '_' | '"' | '\''))
        .collect()
}

fn ensure(state_dir: &Path, version: &str, font: &str) -> PathBuf {
    let path = state_dir.join("home.html");
    let html = HTML
        .replace("{{VERSION}}", version)
        .replace("{{FONT}}", &css_font(font));
    let stale = std::fs::read_to_string(&path)
        .map(|s| s != html)
        .unwrap_or(true);
    if stale {
        let _ = std::fs::create_dir_all(state_dir);
        let _ = std::fs::write(&path, html);
    }
    path
}

pub fn url(state_dir: &Path, version: &str, font: Option<&str>) -> String {
    let font = font
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or(FALLBACK_FONT);
    let path = ensure(state_dir, version, font);
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_font_cannot_escape_the_stylesheet() {
        assert_eq!(
            css_font("Iosevka Term, monospace}body{display:none"),
            "Iosevka Term, monospacebodydisplaynone"
        );
    }

    #[test]
    fn the_page_carries_version_and_font() {
        let dir = std::env::temp_dir().join(format!("kite-home-{}", std::process::id()));
        let url = url(&dir, "9.9.9", Some("Test Mono"));
        assert!(url.starts_with("file://"));
        let html = std::fs::read_to_string(dir.join("home.html")).unwrap();
        assert!(html.contains("kite 9.9.9"));
        assert!(html.contains("Test Mono"));
        assert!(!html.contains("{{FONT}}"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
