//! Small durable store: history, bookmarks, session. JSON on disk, atomic
//! writes, no database engine (an embedded DB would cost more RSS than the
//! whole UI process on the machines this targets).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HistoryEntry {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ts: u64,
    #[serde(default)]
    pub visits: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bookmark {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub ts: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SessionTab {
    pub url: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Session {
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub ts: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Store {
    pub dir: PathBuf,
    pub history: Vec<HistoryEntry>,
    pub bookmarks: Vec<Bookmark>,
    pub session: Option<Session>,
    /// Recently closed windows, newest last. Survives a restart so `u` can
    /// bring back a window that was closed even after kite exited.
    pub closed: Vec<SessionTab>,
}

const HISTORY_CAP: usize = 8000;

fn read_json<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(value) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

impl Store {
    pub fn load(dir: PathBuf) -> Store {
        let _ = std::fs::create_dir_all(&dir);
        Store {
            history: read_json(&dir.join("history.json")).unwrap_or_default(),
            bookmarks: read_json(&dir.join("bookmarks.json")).unwrap_or_default(),
            session: read_json(&dir.join("session.json")),
            closed: read_json(&dir.join("closed.json")).unwrap_or_default(),
            dir,
        }
    }

    pub fn save_closed(&mut self, tabs: &[SessionTab]) {
        self.closed = tabs.to_vec();
        if let Ok(json) = serde_json::to_string(&self.closed) {
            let tmp = self.dir.join("closed.json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("closed.json"));
            }
        }
    }

    /// Take the newest closed window off the stack. Used at boot when the
    /// session left nothing behind, so a quit from the last window is undoable
    /// on the next launch too.
    pub fn pop_closed(&mut self) -> Option<SessionTab> {
        let tab = self.closed.pop()?;
        let rest = self.closed.clone();
        self.save_closed(&rest);
        Some(tab)
    }

    pub fn visit(&mut self, url: &str, title: &str) {
        if url.is_empty() || url == "about:blank" || url.starts_with("devtools://") {
            return;
        }
        let ts = now();
        if let Some(e) = self.history.iter_mut().find(|e| e.url == url) {
            e.ts = ts;
            e.visits += 1;
            if !title.is_empty() {
                e.title = title.to_string();
            }
        } else {
            self.history.push(HistoryEntry {
                url: url.to_string(),
                title: title.to_string(),
                ts,
                visits: 1,
            });
        }
        self.history.sort_by(|a, b| b.ts.cmp(&a.ts));
        self.history.truncate(HISTORY_CAP);
        write_json(&self.dir.join("history.json"), &self.history);
    }

    pub fn set_title(&mut self, url: &str, title: &str) {
        if let Some(e) = self.history.iter_mut().find(|e| e.url == url) {
            if e.title != title {
                e.title = title.to_string();
                write_json(&self.dir.join("history.json"), &self.history);
            }
        }
    }

    /// Most-used + most-recent first: a cheap frecency without an index.
    pub fn search_history(&self, q: &str, limit: usize) -> Vec<&HistoryEntry> {
        let q = q.to_lowercase();
        let mut hits: Vec<&HistoryEntry> = self
            .history
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.url.to_lowercase().contains(&q)
                    || e.title.to_lowercase().contains(&q)
            })
            .collect();
        hits.sort_by(|a, b| {
            let sa = (b.visits as u64).saturating_mul(10) + b.ts.min(1_700_000_000) / 86_400;
            let sb = (a.visits as u64).saturating_mul(10) + a.ts.min(1_700_000_000) / 86_400;
            sa.cmp(&sb)
        });
        hits.truncate(limit);
        hits
    }

    pub fn toggle_bookmark(&mut self, url: &str, title: &str) -> bool {
        let added = if let Some(pos) = self.bookmarks.iter().position(|b| b.url == url) {
            self.bookmarks.remove(pos);
            false
        } else {
            self.bookmarks.insert(
                0,
                Bookmark {
                    url: url.to_string(),
                    title: title.to_string(),
                    ts: now(),
                },
            );
            true
        };
        write_json(&self.dir.join("bookmarks.json"), &self.bookmarks);
        added
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.bookmarks.iter().any(|b| b.url == url)
    }

    pub fn save_session(&mut self, tabs: Vec<SessionTab>, active: usize) {
        let s = Session {
            tabs,
            active,
            ts: now(),
        };
        write_json(&self.dir.join("session.json"), &s);
        self.session = Some(s);
    }

    pub fn bookmarks_json(&self) -> Value {
        json!(self.bookmarks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_stack_survives_a_reload() {
        let dir = std::env::temp_dir().join(format!("kite-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::load(dir.clone());
        s.save_closed(&[SessionTab {
            url: "https://a.example".into(),
            title: "a".into(),
        }]);
        let again = Store::load(dir.clone());
        assert_eq!(again.closed.len(), 1);
        assert_eq!(again.closed[0].url, "https://a.example");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
