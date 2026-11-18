//! The browser shell: owns tabs (one Chromium target each), navigation,
//! history/bookmarks/session, and the privileged side of the in-page command
//! surface. Everything here is invoked from the UI layer over a CDP binding.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cdp::{Cdp, Event};
use crate::config::Config;
use crate::engine::Engine;
use crate::store::{SessionTab, Store};

/// Set from a SIGINT/SIGTERM handler: the window manager closing the last
/// window, `q`, or ^C all leave through the same door so the engine is reaped
/// and the session is written. Without this a `kill` orphans Chromium on the
/// profile and the next launch refuses to start.
static QUIT_SIGNAL: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
}

extern "C" fn note_quit_signal(_sig: i32) {
    QUIT_SIGNAL.store(true, Ordering::SeqCst);
}

/// Route SIGINT/SIGTERM into the main loop instead of dying mid-write.
pub fn install_quit_handler() {
    // SAFETY: the handler only stores into an atomic.
    unsafe {
        signal(2, note_quit_signal);
        signal(15, note_quit_signal);
    }
}

pub struct Tab {
    pub id: String,
    pub session: String,
    /// Execution context of the shell's isolated world in this document.
    /// `None` until the world exists, or after a navigation destroyed it.
    pub world: Option<i64>,
    /// Id of the tab's top frame. The UI script is injected into every frame,
    /// so child frames get `kite` worlds of their own; only a context that
    /// belongs to this frame is the UI.
    pub frame: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub private: bool,
}

pub struct Shell {
    pub cdp: Cdp,
    pub engine: Engine,
    pub cfg: Config,
    pub store: Store,
    pub tabs: Vec<Tab>,
    pub active: usize,
    closed: Vec<SessionTab>,
    /// (label, url) of the extensions loaded by the engine, refreshed whenever
    /// the `:extensions` palette opens.
    ext_cache: Vec<(String, String)>,
    ui: String,
    home: String,
    quit: bool,
    session_dirty: bool,
    last_save: Instant,
    pub startup_mark: Instant,
}

impl Shell {
    pub fn new(
        mut cdp: Cdp,
        engine: Engine,
        cfg: Config,
        store: Store,
        ui: String,
        home: String,
        boot_url: String,
        restore: Vec<String>,
    ) -> Result<Shell, String> {
        let startup_mark = Instant::now();
        cdp.call(
            None,
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
        )?;

        let closed = store.closed.clone();
        let mut shell = Shell {
            cdp,
            engine,
            cfg,
            store,
            tabs: Vec::new(),
            active: 0,
            closed,
            ext_cache: Vec::new(),
            ui,
            home,
            quit: false,
            session_dirty: true,
            last_save: Instant::now(),
            startup_mark,
        };

        let page = shell.find_page_target()?;
        let (session, url) = shell.attach(&page)?;
        shell.tabs.push(Tab {
            id: page,
            session,
            world: None,
            frame: String::new(),
            url,
            title: String::new(),
            loading: true,
            private: false,
        });
        shell.prepare(0)?;
        shell.navigate_active(&boot_url)?;

        for u in restore {
            let _ = shell.new_tab(&u, true, false);
        }
        shell.activate(0);
        shell.handle_downloads();
        Ok(shell)
    }

    fn find_page_target(&mut self) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let r = self.cdp.call(None, "Target.getTargets", json!({}))?;
            if let Some(list) = r.get("targetInfos").and_then(|v| v.as_array()) {
                let pages: Vec<&Value> = list
                    .iter()
                    .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
                    .collect();
                if let Some(t) = pages.first() {
                    if let Some(id) = t.get("targetId").and_then(|x| x.as_str()) {
                        return Ok(id.to_string());
                    }
                }
            }
            if Instant::now() > deadline {
                return Err("engine started but exposed no page target".to_string());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn attach(&mut self, target_id: &str) -> Result<(String, String), String> {
        let r = self.cdp.call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )?;
        let session = r
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| "attach returned no sessionId".to_string())?
            .to_string();
        let info = self
            .cdp
            .call(
                None,
                "Target.getTargetInfo",
                json!({ "targetId": target_id }),
            )
            .unwrap_or(Value::Null);
        let url = info
            .pointer("/targetInfo/url")
            .and_then(|u| u.as_str())
            .unwrap_or("about:blank")
            .to_string();
        Ok((session, url))
    }

    /// Enable the domains we need and install the UI in an isolated world.
    ///
    /// The UI runs in a world of its own, not the page's: the page cannot see
    /// `window.__kite`, cannot call `__kiteSend`, and cannot stop the key
    /// handler. The binding is registered for that context name only, so even
    /// a page that guesses the name has nothing to call.
    fn prepare(&mut self, idx: usize) -> Result<(), String> {
        let session = self.tabs[idx].session.clone();
        let _ = self.cdp.call(Some(&session), "Page.enable", json!({}));
        let _ = self.cdp.call(Some(&session), "Runtime.enable", json!({}));
        let _ = self.cdp.call(Some(&session), "Log.enable", json!({}));
        if self.cfg.get_bool("javascript") == Some(false) {
            // Page scripts off; the shell's own world is not page script.
            let _ = self.cdp.call(
                Some(&session),
                "Emulation.setScriptExecutionDisabled",
                json!({ "value": true }),
            );
        }
        if crate::engine::env_flag("KITE_NO_UI") {
            return Ok(());
        }
        let _ = self.cdp.call(
            Some(&session),
            "Runtime.addBinding",
            json!({ "name": "__kiteSend", "executionContextName": "kite" }),
        );
        let _ = self.cdp.call(
            Some(&session),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": self.ui, "worldName": "kite" }),
        );
        // The current document predates the injection: make its world now.
        self.ensure_world(idx);
        self.inject_now(idx);
        Ok(())
    }

    /// The id of the tab's top frame, cached once known. A target keeps the
    /// same top frame for its whole life; only the document inside it changes.
    fn main_frame(&mut self, idx: usize) -> Option<String> {
        if !self.tabs[idx].frame.is_empty() {
            return Some(self.tabs[idx].frame.clone());
        }
        let session = self.tabs[idx].session.clone();
        let frame = self
            .cdp
            .call(Some(&session), "Page.getFrameTree", json!({}))
            .ok()
            .and_then(|t| {
                t.pointer("/frameTree/frame/id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if frame.is_empty() {
            return None;
        }
        self.tabs[idx].frame = frame.clone();
        Some(frame)
    }

    /// Create the `kite` world for the tab's *current* document and remember
    /// its context id.
    ///
    /// Worlds are per document and `addScriptToEvaluateOnNewDocument` only
    /// runs in one that already exists, so every navigation needs a fresh
    /// `createIsolatedWorld`. Idempotent: Chromium hands back the same context
    /// if the world is already there.
    fn ensure_world(&mut self, idx: usize) {
        if self.tabs[idx].world.is_some() {
            return;
        }
        let session = self.tabs[idx].session.clone();
        let Some(frame) = self.main_frame(idx) else {
            return;
        };
        if let Ok(w) = self.cdp.call(
            Some(&session),
            "Page.createIsolatedWorld",
            json!({ "frameId": frame, "worldName": "kite", "grantUniveralAccess": false }),
        ) {
            self.tabs[idx].world = w.get("executionContextId").and_then(|v| v.as_i64());
        }
    }

    fn inject_now(&mut self, idx: usize) {
        let session = self.tabs[idx].session.clone();
        let ui = self.ui.clone();
        self.ensure_world(idx);
        let Some(world) = self.inject_ui(idx, &session, &ui) else {
            return;
        };
        let boot = self.boot_payload(idx);
        let push = format!(
            "(()=>{{try{{window.__kite&&window.__kite.boot({})}}catch(e){{}}}})()",
            boot
        );
        let _ = self.cdp.eval_ctx(Some(&session), Some(world), &push);
    }

    /// Evaluate the UI layer in the tab's world, recreating the world once if
    /// the context id has gone stale (a navigation raced the events).
    fn inject_ui(&mut self, idx: usize, session: &str, ui: &str) -> Option<i64> {
        if let Some(world) = self.tabs[idx].world {
            if self.cdp.eval_ctx(Some(session), Some(world), ui).is_ok() {
                return Some(world);
            }
        }
        self.tabs[idx].world = None;
        self.ensure_world(idx);
        let world = self.tabs[idx].world?;
        self.cdp.eval_ctx(Some(session), Some(world), ui).ok()?;
        Some(world)
    }

    /// Evaluate in the shell's world; a no-op while the world is missing, never
    /// falls back to the page's world.
    fn world_eval(&mut self, session: &str, expr: &str) -> Result<Value, String> {
        let ctx = self
            .tabs
            .iter()
            .find(|t| t.session == session)
            .and_then(|t| t.world);
        match ctx {
            Some(c) => self.cdp.eval_ctx(Some(session), Some(c), expr),
            None => Ok(Value::Null),
        }
    }

    /// For tests: run something in the shell's world of the active tab.
    pub fn eval_ui(&mut self, expr: &str) -> Result<Value, String> {
        let Some(session) = self.active_session() else {
            return Err("no active tab".to_string());
        };
        self.world_eval(&session, expr)
    }

    /// For tests: run something in the page's own world.
    pub fn eval_page(&mut self, expr: &str) -> Result<Value, String> {
        let Some(session) = self.active_session() else {
            return Err("no active tab".to_string());
        };
        self.cdp.eval(Some(&session), expr)
    }

    fn boot_payload(&self, idx: usize) -> String {
        let extra = json!({
            "tabs": self.tabs_json(),
            "activeIndex": idx,
            "url": self.tabs.get(idx).map(|t| t.url.clone()).unwrap_or_default(),
            "title": self.tabs.get(idx).map(|t| t.title.clone()).unwrap_or_default(),
            "bookmarked": self
                .tabs
                .get(idx)
                .map(|t| self.store.is_bookmarked(&t.url))
                .unwrap_or(false),
            "home": self.home.clone(),
        });
        serde_json::to_string(&self.cfg.ui_json(extra)).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn tabs_json(&self) -> Value {
        let items: Vec<Value> = self
            .tabs
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "url": t.url,
                    "title": if t.title.is_empty() { t.url.clone() } else { t.title.clone() },
                    "loading": t.loading,
                    "private": t.private,
                })
            })
            .collect();
        Value::Array(items)
    }

    fn active_session(&self) -> Option<String> {
        self.tabs.get(self.active).map(|t| t.session.clone())
    }

    /// Push fresh tab/nav state to the visible page's UI layer.
    pub fn push_state(&mut self) {
        let Some(session) = self.active_session() else {
            return;
        };
        let idx = self.active;
        let payload = json!({
            "tabs": self.tabs_json(),
            "activeIndex": idx,
            "url": self.tabs[idx].url.clone(),
            "title": self.tabs[idx].title.clone(),
            "loading": self.tabs[idx].loading,
            "bookmarked": self.store.is_bookmarked(&self.tabs[idx].url),
            "private": self.tabs[idx].private,
        });
        let expr = format!(
            "(()=>{{try{{window.__kite&&window.__kite.state({})}}catch(e){{}}}})()",
            payload
        );
        let _ = self.world_eval(&session, &expr);
    }

    pub fn toast(&mut self, msg: &str) {
        let Some(session) = self.active_session() else {
            return;
        };
        let expr = format!(
            "(()=>{{try{{window.__kite&&window.__kite.toast({})}}catch(e){{}}}})()",
            json!(msg)
        );
        let _ = self.world_eval(&session, &expr);
    }

    pub fn navigate_active(&mut self, url: &str) -> Result<(), String> {
        let session = self.active_session().ok_or("no active tab")?;
        let parsed = self.resolve_url(url);
        self.cdp
            .call(Some(&session), "Page.navigate", json!({ "url": parsed }))?;
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.url = parsed;
            t.loading = true;
        }
        self.session_dirty = true;
        Ok(())
    }

    pub fn new_tab(&mut self, url: &str, background: bool, private: bool) -> Result<usize, String> {
        let parsed = self.resolve_url(url);
        let blank = parsed == "about:blank";
        // Preferred path: let the live browser open a real app window. A CDP
        // `Target.createTarget` always builds a tabbed browser window, which
        // means the tab strip and omnibox the shell exists to avoid.
        if !private {
            if let Some(idx) = self.open_app_window(&parsed, blank)? {
                self.finish_tab(idx, background);
                return Ok(idx);
            }
        }
        // Private tabs need a CDP browser context, and a fallback in case the
        // engine refuses the extra window.
        self.new_tab_cdp(&parsed, blank, background, private)
    }

    fn finish_tab(&mut self, idx: usize, background: bool) {
        if let Err(e) = self.prepare(idx) {
            eprintln!("kite: tab prepare failed: {e}");
        }
        if !background {
            self.activate(idx);
        }
        self.session_dirty = true;
    }

    /// Spawn `--app=<url>` on the live profile and adopt the target the browser
    /// creates for it. `None` when no new page target shows up in time.
    fn open_app_window(&mut self, url: &str, blank: bool) -> Result<Option<usize>, String> {
        let before = self.page_targets()?;
        self.engine.open_app_window(url)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let known = self.page_targets()?;
            if let Some((id, target_url)) = known
                .iter()
                .find(|(id, _)| !before.iter().any(|(b, _)| b == id))
                .cloned()
            {
                let (session, u) = self.attach(&id)?;
                self.tabs.push(Tab {
                    id,
                    session,
                    world: None,
                    frame: String::new(),
                    url: if blank { "about:blank".to_string() } else { u },
                    title: String::new(),
                    loading: true,
                    private: false,
                });
                let idx = self.tabs.len() - 1;
                if blank {
                    let session = self.tabs[idx].session.clone();
                    let _ = self.cdp.call(
                        Some(&session),
                        "Page.navigate",
                        json!({ "url": "about:blank" }),
                    );
                }
                let _ = target_url;
                return Ok(Some(idx));
            }
            std::thread::sleep(Duration::from_millis(120));
        }
        Ok(None)
    }

    fn page_targets(&mut self) -> Result<Vec<(String, String)>, String> {
        let r = self.cdp.call(None, "Target.getTargets", json!({}))?;
        let mut out = Vec::new();
        for t in r
            .get("targetInfos")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if t.get("type").and_then(|v| v.as_str()) != Some("page") {
                continue;
            }
            let Some(id) = t.get("targetId").and_then(|v| v.as_str()) else {
                continue;
            };
            let url = t
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push((id.to_string(), url));
        }
        Ok(out)
    }

    fn new_tab_cdp(
        &mut self,
        parsed: &str,
        blank: bool,
        background: bool,
        private: bool,
    ) -> Result<usize, String> {
        let create_url = if blank {
            crate::engine::PLACEHOLDER
        } else {
            parsed
        };
        let mut params = json!({ "url": create_url, "background": true, "newWindow": false });
        if private {
            let ctx = self
                .cdp
                .call(None, "Target.createBrowserContext", json!({}))?;
            if let Some(id) = ctx.get("browserContextId").and_then(|v| v.as_str()) {
                params["browserContextId"] = json!(id);
            }
        }
        let r = self.cdp.call(None, "Target.createTarget", params)?;
        let target_id = r
            .get("targetId")
            .and_then(|t| t.as_str())
            .ok_or("createTarget returned no targetId")?
            .to_string();
        let (session, u) = self.attach(&target_id)?;
        self.tabs.push(Tab {
            id: target_id,
            session,
            world: None,
            frame: String::new(),
            url: if blank { "about:blank".to_string() } else { u },
            title: String::new(),
            loading: true,
            private,
        });
        let idx = self.tabs.len() - 1;
        if blank {
            let session = self.tabs[idx].session.clone();
            let _ = self.cdp.call(
                Some(&session),
                "Page.navigate",
                json!({ "url": "about:blank" }),
            );
        }
        self.finish_tab(idx, background);
        Ok(idx)
    }

    pub fn activate(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let id = self.tabs[idx].id.clone();
        let _ = self
            .cdp
            .call(None, "Target.activateTarget", json!({ "targetId": id }));
        self.active = idx;
        self.session_dirty = true;
        self.push_state();
        self.remember_visit();
    }

    pub fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(idx);
        self.remember_closed(&tab.url, &tab.title);
        let _ = self
            .cdp
            .call(None, "Target.closeTarget", json!({ "targetId": tab.id }));
        if self.tabs.is_empty() {
            self.quit = true;
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.session_dirty = true;
        self.push_state();
    }

    fn remember_closed(&mut self, url: &str, title: &str) {
        if url.is_empty() || url == "about:blank" {
            return;
        }
        self.closed.push(SessionTab {
            url: url.to_string(),
            title: title.to_string(),
        });
        if self.closed.len() > 20 {
            self.closed.remove(0);
        }
        self.store.save_closed(&self.closed);
    }

    fn remember_visit(&mut self) {
        if let Some(t) = self.tabs.get(self.active) {
            let (u, title) = (t.url.clone(), t.title.clone());
            self.store.visit(&u, &title);
        }
    }

    /// "example.com" -> https URL, "ddg foo" -> engine query, "rust async" -> search.
    pub fn resolve_url(&self, input: &str) -> String {
        resolve_input(&self.cfg, input, &self.home)
    }

    fn handle_downloads(&mut self) {
        let dir = std::env::var("HOME")
            .map(|h| format!("{h}/Downloads"))
            .unwrap_or_else(|_| "/tmp".to_string());
        let _ = self.cdp.call(
            None,
            "Browser.setDownloadBehavior",
            json!({ "behavior": "allow", "downloadPath": dir, "eventsEnabled": true }),
        );
    }

    fn save_session_if_due(&mut self, force: bool) {
        if !self.session_dirty && !force {
            return;
        }
        if !force && self.last_save.elapsed() < Duration::from_secs(3) {
            return;
        }
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .map(|t| SessionTab {
                url: t.url.clone(),
                title: t.title.clone(),
            })
            .collect();
        if !tabs.is_empty() {
            self.store.save_session(tabs, self.active);
        }
        self.last_save = Instant::now();
        self.session_dirty = false;
    }

    /// Wait for the first real document load of the boot page. Used by
    /// --bench/--selftest, and to avoid reporting about:blank as first paint.
    pub fn wait_first_paint(&mut self, block: bool) -> Option<Duration> {
        if !block {
            return None;
        }
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut navigated = false;
        while Instant::now() < deadline {
            let Some(ev) = self.cdp.wait_event(Duration::from_millis(250)) else {
                if !self.engine.alive() {
                    return None;
                }
                continue;
            };
            let is_nav = ev.method == "Page.frameNavigated";
            let nav_url = ev
                .params
                .pointer("/frame/url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_load = ev.method == "Page.loadEventFired";
            self.handle_event(ev);
            if is_nav && !nav_url.is_empty() && nav_url != "about:blank" {
                navigated = true;
            }
            if is_load && navigated {
                return Some(self.startup_mark.elapsed());
            }
        }
        None
    }

    /// Poll and handle engine events for a bounded time (tests, and startup
    /// work that must not miss bindings).
    pub fn pump(&mut self, dur: Duration) {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if let Some(ev) = self.cdp.wait_event(Duration::from_millis(20)) {
                self.handle_event(ev);
            }
            for _ in 0..32 {
                match self.cdp.wait_event(Duration::from_millis(0)) {
                    Some(ev) => self.handle_event(ev),
                    None => break,
                }
            }
        }
    }

    pub fn run(&mut self) -> Result<(), String> {
        loop {
            if self.quit || QUIT_SIGNAL.load(Ordering::Relaxed) {
                break;
            }
            match self.cdp.wait_event(Duration::from_millis(250)) {
                Some(ev) => self.handle_event(ev),
                None => {
                    if !self.engine.alive() {
                        return Err("engine exited".to_string());
                    }
                }
            }
            for _ in 0..64 {
                let Some(ev) = self.cdp.wait_event(Duration::from_millis(0)) else {
                    break;
                };
                self.handle_event(ev);
            }
            self.save_session_if_due(false);
        }
        self.save_session_if_due(true);
        Ok(())
    }

    fn handle_event(&mut self, ev: Event) {
        match ev.method.as_str() {
            "Runtime.bindingCalled" => {
                let name = ev.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name == "__kiteSend" {
                    if let Some(payload) = ev.params.get("payload").and_then(|v| v.as_str()) {
                        self.handle_binding(ev.session.clone(), payload);
                    }
                }
            }
            "Page.frameNavigated" => {
                if ev.params.pointer("/frame/parentId").is_none() {
                    let url = ev
                        .params
                        .pointer("/frame/url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Some(idx) = self.tab_by_session(&ev.session) {
                        self.tabs[idx].url = url;
                        self.tabs[idx].loading = true;
                        if let Some(id) = ev.params.pointer("/frame/id").and_then(|v| v.as_str()) {
                            self.tabs[idx].frame = id.to_string();
                        }
                        // New document, new world: the context id from the old
                        // one is dead, so drop it and rebuild before anything
                        // in the shell tries to evaluate.
                        self.tabs[idx].world = None;
                        self.inject_now(idx);
                        if self.active == idx {
                            self.push_state();
                        }
                        self.session_dirty = true;
                    }
                }
            }
            "Page.loadEventFired" => {
                if let Some(idx) = self.tab_by_session(&ev.session) {
                    // The world is per document; make sure the UI is in it even
                    // if the new-document script did not run.
                    self.inject_now(idx);
                    self.tabs[idx].loading = false;
                    if self.active == idx {
                        self.remember_visit();
                        self.push_state();
                    }
                }
                self.session_dirty = true;
            }
            "Target.targetInfoChanged" => {
                if let Some(info) = ev.params.get("targetInfo") {
                    let id = info.get("targetId").and_then(|v| v.as_str()).unwrap_or("");
                    let title = info.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let url = info.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
                        if !title.is_empty() {
                            self.tabs[idx].title = title.to_string();
                        }
                        if !url.is_empty() && url != "about:blank" {
                            self.tabs[idx].url = url.to_string();
                        }
                        if self.active == idx {
                            self.push_state();
                        }
                    }
                }
            }
            "Target.targetDestroyed" => {
                let id = ev
                    .params
                    .get("targetId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(idx) = self.tabs.iter().position(|t| t.id == id) {
                    // Engine-side close: window manager close, crash, a page
                    // calling window.close(). Keep it recoverable with `u`.
                    let (url, title) = (self.tabs[idx].url.clone(), self.tabs[idx].title.clone());
                    if self.tabs.len() == 1 {
                        self.remember_closed(&url, &title);
                        self.save_session_if_due(true);
                        self.quit = true;
                        return;
                    }
                    self.remember_closed(&url, &title);
                    self.tabs.remove(idx);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len() - 1;
                    }
                    self.session_dirty = true;
                    self.push_state();
                }
            }
            "Runtime.executionContextCreated" => {
                let ctx = &ev.params["context"];
                if ctx.get("name").and_then(|v| v.as_str()) == Some("kite") {
                    let frame = ctx
                        .pointer("/auxData/frameId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if let Some(idx) = self.tab_by_session(&ev.session) {
                        // The UI script is injected into every frame, so a
                        // frame with an ad in it hands us its own `kite` world
                        // too. Adopting that id would aim every reply at the
                        // iframe — the palette would open and never fill.
                        let main = self.main_frame(idx);
                        if main.as_deref() == Some(frame.as_str()) {
                            self.tabs[idx].world = ctx.get("id").and_then(|v| v.as_i64());
                        }
                    }
                }
            }
            "Runtime.executionContextsCleared" => {
                if let Some(idx) = self.tab_by_session(&ev.session) {
                    self.tabs[idx].world = None;
                }
            }
            "Page.javascriptDialogOpening" => {
                // `alert` and `beforeunload` are informational; `confirm` and
                // `prompt` decide whether something happens, so they are
                // blocked by default. A page must never get a free "yes".
                let kind = ev.params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let allow = self.cfg.get_str("dialogs").unwrap_or("block") == "allow"
                    || matches!(kind, "alert" | "beforeunload");
                let _ = self.cdp.call(
                    ev.session.as_deref(),
                    "Page.handleJavaScriptDialog",
                    json!({ "accept": allow }),
                );
                if !allow {
                    self.toast(&format!("blocked {kind} dialog"));
                }
            }
            "Page.windowOpen" => {
                if let Some(url) = ev.params.get("url").and_then(|v| v.as_str()) {
                    if !url.is_empty() {
                        let _ = self.new_tab(url, false, false);
                    }
                }
            }
            "Browser.downloadWillBegin" => {
                let name = ev
                    .params
                    .get("suggestedFilename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("file");
                self.toast(&format!("downloading {name}"));
            }
            "Inspector.targetCrashed" => self.toast("page crashed"),
            _ => {}
        }
    }

    fn tab_by_session(&self, session: &Option<String>) -> Option<usize> {
        let s = session.as_deref()?;
        self.tabs.iter().position(|t| t.session == s)
    }

    fn reply(&mut self, session: Option<String>, rid: Value, body: Value) {
        let Some(s) = session else { return };
        // Never drop a reply on the floor: when the world id is missing (the
        // document changed under us) rebuild it before answering, or the
        // palette would sit empty with no error anywhere.
        if let Some(idx) = self.tab_by_session(&Some(s.clone())) {
            if self.tabs[idx].world.is_none() {
                self.ensure_world(idx);
            }
        }
        let expr = format!(
            "(()=>{{try{{window.__kite&&window.__kite.recv({}, {})}}catch(e){{}}}})()",
            rid, body
        );
        let _ = self.world_eval(&s, &expr);
    }

    fn handle_binding(&mut self, session: Option<String>, payload: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        let cmd = msg.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        if crate::engine::env_flag("KITE_DEBUG_BIND") {
            eprintln!(
                "[bind] cmd={cmd} session={:?}",
                session.as_deref().map(|s| &s[..8.min(s.len())])
            );
        }
        let args = msg.get("args").cloned().unwrap_or(Value::Null);
        let rid = msg.get("rid").cloned().unwrap_or(Value::Null);
        let arg_str = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let arg_u = |k: &str| args.get(k).and_then(|v| v.as_u64()).map(|n| n as usize);
        let arg_b = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);

        match cmd {
            "hello" => {
                // A fresh document booted: hand it config + tab state.
                let idx = self.tab_by_session(&session).unwrap_or(self.active);
                let boot = self.boot_payload(idx);
                let Some(s) = session.clone() else { return };
                let expr = format!(
                    "(()=>{{try{{window.__kite&&window.__kite.boot({})}}catch(e){{}}}})()",
                    boot
                );
                let _ = self.world_eval(&s, &expr);
            }
            "nav" => {
                let url = arg_str("url");
                let new_tab = arg_b("newTab");
                let background = arg_b("background");
                let private = arg_b("private");
                let res = if new_tab {
                    self.new_tab(&url, background, private).map(|_| Value::Null)
                } else {
                    self.navigate_active(&url).map(|_| Value::Null)
                };
                match res {
                    Ok(_) => self.reply(session, rid, json!({ "ok": true })),
                    Err(e) => self.reply(session, rid, json!({ "ok": false, "error": e })),
                }
            }
            "tab" => {
                let op = arg_str("op");
                let n = self.tabs.len();
                match op.as_str() {
                    "new" => {
                        let url = args
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("about:blank");
                        if let Err(e) = self.new_tab(url, false, arg_b("private")) {
                            eprintln!("kite: tab new failed: {e}");
                            self.toast(&format!("tab failed: {e}"));
                        }
                    }
                    "close" => self.close_tab(arg_u("index").unwrap_or(self.active)),
                    "next" => self.activate((self.active + 1) % n.max(1)),
                    "prev" => self.activate((self.active + n.max(1) - 1) % n.max(1)),
                    "first" => self.activate(0),
                    "last" => self.activate(n.saturating_sub(1)),
                    "select" => self.activate(arg_u("index").unwrap_or(0)),
                    "undo" => {
                        if let Some(c) = self.closed.pop() {
                            let label = if c.title.is_empty() {
                                c.url.clone()
                            } else {
                                c.title.clone()
                            };
                            let _ = self.new_tab(&c.url, false, false);
                            self.toast(&format!("reopened {label}"));
                        } else {
                            self.toast("nothing to reopen");
                        }
                    }
                    _ => {}
                }
                self.reply(
                    session,
                    rid,
                    json!({ "ok": true, "tabs": self.tabs_json() }),
                );
            }
            "history" => {
                let op = arg_str("op");
                let Some(s) = session.clone() else { return };
                let js = match op.as_str() {
                    "back" => "history.back()",
                    "forward" => "history.forward()",
                    "reload" => "location.reload()",
                    "hardreload" => "location.reload()",
                    "stop" => "window.stop()",
                    _ => "",
                };
                if op == "hardreload" {
                    if let Some(t) = self.tabs.iter().find(|t| t.session == s) {
                        let url = t.url.clone();
                        let _ =
                            self.cdp
                                .call(Some(&s), "Page.reload", json!({ "ignoreCache": true }));
                        let _ = url;
                    }
                } else if !js.is_empty() {
                    let _ = self.cdp.eval(Some(&s), js);
                }
                self.reply(session, rid, json!({ "ok": true }));
            }
            "complete" => {
                let kind = arg_str("kind");
                let q = arg_str("query");
                let limit = arg_u("limit").unwrap_or(12);
                if kind == "ext" {
                    self.ext_cache = self.discover_extensions();
                    if self.ext_cache.is_empty() {
                        // An empty list otherwise looks like "extensions are
                        // broken"; the common case is that none are configured.
                        let msg = if self.cfg.extensions().is_empty() {
                            "no extensions configured: set general.extensions"
                        } else {
                            "no loaded extensions detected"
                        };
                        self.toast(msg);
                    }
                }
                let items = self.completions(&kind, &q, limit);
                self.reply(session, rid, json!({ "items": items }));
            }
            "yank" => {
                let text = arg_str("text");
                let ok = clipboard_write(&text);
                self.toast(if ok {
                    "yanked"
                } else {
                    "clipboard unavailable"
                });
                self.reply(session, rid, json!({ "ok": ok }));
            }
            "paste" => {
                let text = clipboard_read().unwrap_or_default();
                self.reply(session, rid, json!(text));
            }
            "bookmark" => {
                let op = arg_str("op");
                let url = if arg_str("url").is_empty() {
                    self.current_url()
                } else {
                    arg_str("url")
                };
                let title = if arg_str("title").is_empty() {
                    self.current_title()
                } else {
                    arg_str("title")
                };
                match op.as_str() {
                    "toggle" => {
                        let added = self.store.toggle_bookmark(&url, &title);
                        self.toast(if added {
                            "bookmarked"
                        } else {
                            "bookmark removed"
                        });
                        self.push_state();
                    }
                    _ => {}
                }
                self.reply(
                    session,
                    rid,
                    json!({ "ok": true, "bookmarks": self.store.bookmarks_json() }),
                );
            }
            "quit" => {
                self.quit = true;
                self.save_session_if_due(true);
            }
            "external" => {
                let url = if arg_str("url").is_empty() {
                    self.current_url()
                } else {
                    arg_str("url")
                };
                let _ = std::process::Command::new("xdg-open").arg(url).spawn();
            }
            "title" => {
                let title = arg_str("title");
                let url = self.current_url();
                if !title.is_empty() {
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.title = title.clone();
                    }
                    self.store.set_title(&url, &title);
                }
            }
            "ready" => {
                self.push_state();
            }
            _ => {}
        }
    }

    fn current_url(&self) -> String {
        self.tabs
            .get(self.active)
            .map(|t| t.url.clone())
            .unwrap_or_default()
    }

    fn current_title(&self) -> String {
        self.tabs
            .get(self.active)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    }

    /// The extensions the engine actually loaded, as (label, url of their UI).
    ///
    /// Chromium derives an unpacked extension's id from its directory path, so
    /// the id is stable but impossible to guess. The profile's `Preferences`
    /// records the id -> directory mapping for every extension, asleep or not;
    /// the live targets are a fallback for the very first run, before that file
    /// is written.
    fn discover_extensions(&mut self) -> Vec<(String, String)> {
        let dirs = self.cfg.extensions();
        let mut out: Vec<(String, String)> = Vec::new();
        if dirs.is_empty() {
            return out;
        }
        let prefs = self.engine.profile.join("Default/Preferences");
        let by_path: Vec<(String, String)> = std::fs::read_to_string(&prefs)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                v.pointer("/extensions/settings")
                    .and_then(|s| s.as_object().cloned())
            })
            .map(|m| {
                m.iter()
                    .filter_map(|(id, e)| {
                        let p = e.get("path").and_then(|p| p.as_str())?;
                        Some((id.clone(), p.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let by_target = self.extension_targets();
        for dir in &dirs {
            let manifest = read_manifest(Path::new(dir));
            let bg = manifest.as_ref().map(background_path).unwrap_or_default();
            let id = by_path
                .iter()
                .find(|(_, p)| same_path(p) == same_path(dir))
                .map(|(id, _)| id.clone())
                .or_else(|| {
                    by_target
                        .iter()
                        .find(|(_, path)| !bg.is_empty() && *path == bg)
                        .map(|(id, _)| id.clone())
                })
                .or_else(|| {
                    // One extension, one id: there is nothing to confuse.
                    if dirs.len() == 1 {
                        by_target.first().map(|(id, _)| id.clone())
                    } else {
                        None
                    }
                });
            let Some(id) = id else { continue };
            let d = Path::new(dir);
            let (label, page) = match &manifest {
                Some(m) => (ext_name(d, m), ext_page(d, m)),
                None => (dir_label(d), String::new()),
            };
            let url = format!("chrome-extension://{id}/{page}");
            if !out.iter().any(|(_, u)| *u == url) {
                out.push((label, url));
            }
        }
        out
    }

    /// `(id, script path)` for every extension with a live target.
    fn extension_targets(&mut self) -> Vec<(String, String)> {
        self.cdp
            .call(None, "Target.getTargets", json!({}))
            .ok()
            .and_then(|v| v.get("targetInfos").and_then(|t| t.as_array().cloned()))
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("url").and_then(|u| u.as_str()))
                    .filter_map(|u| {
                        let rest = u.strip_prefix("chrome-extension://")?;
                        let (id, path) = rest.split_once('/')?;
                        Some((id.to_string(), path.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn completions(&self, kind: &str, q: &str, limit: usize) -> Vec<Value> {
        let ql = q.to_lowercase();
        let mut out: Vec<Value> = Vec::new();
        match kind {
            "command" => {
                for (name, desc) in COMMANDS {
                    if ql.is_empty() || fuzzy(name, &ql) || desc.contains(&ql) {
                        out.push(json!({ "text": name, "detail": desc, "kind": "command" }));
                    }
                }
            }
            "bookmark" => {
                for b in &self.store.bookmarks {
                    if ql.is_empty()
                        || b.url.to_lowercase().contains(&ql)
                        || b.title.to_lowercase().contains(&ql)
                    {
                        let t = if b.title.is_empty() {
                            b.url.clone()
                        } else {
                            b.title.clone()
                        };
                        out.push(json!({ "text": t, "detail": b.url, "kind": "bookmark" }));
                    }
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            "ext" => {
                for (label, url) in &self.ext_cache {
                    if ql.is_empty()
                        || label.to_lowercase().contains(&ql)
                        || url.to_lowercase().contains(&ql)
                    {
                        out.push(json!({ "text": label, "detail": url, "kind": "ext" }));
                    }
                }
            }
            "tab" => {
                for (i, t) in self.tabs.iter().enumerate() {
                    let label = if t.title.is_empty() {
                        t.url.clone()
                    } else {
                        t.title.clone()
                    };
                    if ql.is_empty()
                        || label.to_lowercase().contains(&ql)
                        || t.url.to_lowercase().contains(&ql)
                    {
                        out.push(
                            json!({ "text": label, "detail": t.url, "kind": "tab", "index": i }),
                        );
                    }
                }
            }
            // default: the omnibox — engines, bookmarks, history, then the raw query
            _ => {
                if ql.is_empty() {
                    for (name, tmpl) in &self.cfg.search {
                        out.push(json!({ "text": format!("{name}: "), "detail": tmpl, "kind": "engine" }));
                    }
                }
                for b in &self.store.bookmarks {
                    if !ql.is_empty()
                        && (b.url.to_lowercase().contains(&ql)
                            || b.title.to_lowercase().contains(&ql))
                    {
                        let t = if b.title.is_empty() {
                            b.url.clone()
                        } else {
                            b.title.clone()
                        };
                        out.push(json!({ "text": t, "detail": b.url, "kind": "bookmark" }));
                    }
                    if out.len() >= limit {
                        break;
                    }
                }
                for h in self.store.search_history(&q, 8) {
                    let t = if h.title.is_empty() {
                        h.url.clone()
                    } else {
                        h.title.clone()
                    };
                    out.push(json!({ "text": t, "detail": h.url, "kind": "history" }));
                }
                if !q.is_empty() {
                    let t = interpret_input(&self.cfg, &q, &self.home);
                    out.push(match t.kind {
                        InputKind::Home => json!({
                            "text": "home",
                            "detail": t.url,
                            "kind": "url",
                            "url": t.url,
                        }),
                        InputKind::Url => json!({
                            "text": t.url,
                            "detail": "open",
                            "kind": "url",
                            "url": t.url,
                        }),
                        InputKind::Engine => json!({
                            "text": format!("{}: {}", t.engine, t.query),
                            "detail": t.url,
                            "kind": "engine",
                            "url": t.url,
                        }),
                        InputKind::Search => json!({
                            "text": format!("{}: {}", t.engine, t.query),
                            "detail": t.url,
                            "kind": "search",
                            "url": t.url,
                        }),
                    });
                }
            }
        }
        out.truncate(limit);
        out
    }
}

/// What the omnibox decided to do with an input. The palette uses the same
/// classification as navigation, so the row it shows is what Enter does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    Home,
    Url,
    Search,
    Engine,
}

pub struct InputTarget {
    pub kind: InputKind,
    pub url: String,
    /// Engine name for `Search` (the configured default) and `Engine` (the
    /// typed prefix); empty otherwise.
    pub engine: String,
    /// The query for `Search`, or the text after an engine prefix.
    pub query: String,
}

fn search_url(cfg: &Config, engine_name: &str, query: &str) -> String {
    let engine = cfg
        .search
        .get(engine_name)
        .and_then(|v| v.as_str())
        .unwrap_or("https://duckduckgo.com/?q={}");
    engine.replace("{}", &urlencode(query))
}

/// A scheme-less input is a host only when it has a plausible hostname: a dot
/// followed by an alphabetic TLD, a port, or an IPv4 literal. This keeps
/// `lemonde.fr` a URL while `3.14` and `foo:bar` stay searches.
fn looks_like_host(s: &str) -> bool {
    if s.is_empty() || s.chars().any(char::is_whitespace) || s.ends_with('.') {
        return false;
    }
    let authority = s.split(['/', '?', '#']).next().unwrap_or(s);
    let (host, had_port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            (host, true)
        }
        None => (authority, false),
    };
    if host.is_empty() {
        return false;
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return false;
    }
    // An explicit port makes an intranet name (`buildbox:8080`) a host even
    // without a dot.
    if had_port {
        return true;
    }
    let Some((name, tld)) = host.rsplit_once('.') else {
        return false;
    };
    !name.is_empty() && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// The single interpretation shared by navigation and the omnibox preview.
fn interpret_input(cfg: &Config, input: &str, home: &str) -> InputTarget {
    let s = input.trim();
    let home_target = || InputTarget {
        kind: InputKind::Home,
        url: home.to_string(),
        engine: String::new(),
        query: String::new(),
    };
    if s == "home" || s == "kite:home" {
        return home_target();
    }
    if s.is_empty() {
        let start = cfg.get_str("start_page").map(str::trim).unwrap_or("home");
        if start.is_empty() || matches!(start, "home" | "kite" | "kite:home" | "about:blank") {
            return home_target();
        }
        return interpret_input(cfg, start, home);
    }
    if s.starts_with("javascript:") {
        return home_target();
    }
    let has_scheme = s.contains("://")
        || s.starts_with("about:")
        || s.starts_with("file:")
        || s.starts_with("data:")
        || s.starts_with("view-source:");
    if has_scheme {
        return InputTarget {
            kind: InputKind::Url,
            url: s.to_string(),
            engine: String::new(),
            query: s.to_string(),
        };
    }
    if s.starts_with("localhost") || s.starts_with("127.0.0.1") || s.starts_with("[::1]") {
        return InputTarget {
            kind: InputKind::Url,
            url: format!("http://{s}"),
            engine: String::new(),
            query: s.to_string(),
        };
    }
    if let Some((head, rest)) = s.split_once(' ') {
        if cfg.search.get(head).and_then(|v| v.as_str()).is_some() {
            return InputTarget {
                kind: InputKind::Engine,
                url: search_url(cfg, head, rest),
                engine: head.to_string(),
                query: rest.to_string(),
            };
        }
    }
    if looks_like_host(s) {
        return InputTarget {
            kind: InputKind::Url,
            url: format!("https://{s}"),
            engine: String::new(),
            query: s.to_string(),
        };
    }
    let engine = cfg.get_str("search_engine").unwrap_or("ddg").to_string();
    InputTarget {
        kind: InputKind::Search,
        url: search_url(cfg, &engine, s),
        engine,
        query: s.to_string(),
    }
}

/// Shared by the shell and by startup, so a scheme-less `start_page` or CLI
/// argument is normalised once and the engine's `--app=` window is never
/// opened on a URL Chromium would reject.
pub fn resolve_input(cfg: &Config, input: &str, home: &str) -> String {
    interpret_input(cfg, input, home).url
}

fn read_manifest(dir: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn background_path(m: &Value) -> String {
    let path = m
        .pointer("/background/service_worker")
        .or_else(|| m.pointer("/background/page"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Manifests may write `/js/background.js`, while target URLs never carry
    // the leading slash. Normalise so the first-run id lookup can match.
    path.trim_start_matches('/').to_string()
}

/// Two spellings of the same directory compare equal (`~`, symlinks, a
/// trailing slash).
fn same_path(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.trim_end_matches('/').to_string())
}

fn dir_label(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string())
}

/// `__MSG_extName__` -> the localized message, one level deep.
fn ext_name(dir: &Path, m: &Value) -> String {
    let raw = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let key = raw
        .strip_prefix("__MSG_")
        .and_then(|r| r.strip_suffix("__"));
    if let Some(key) = key {
        let loc = m
            .get("default_locale")
            .and_then(|v| v.as_str())
            .unwrap_or("en");
        let path = dir.join("_locales").join(loc).join("messages.json");
        if let Ok(txt) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if let Some(msg) = v
                    .get(key)
                    .and_then(|e| e.get("message"))
                    .and_then(|s| s.as_str())
                {
                    return msg.to_string();
                }
            }
        }
        return dir_label(dir);
    }
    if raw.is_empty() {
        dir_label(dir)
    } else {
        raw.to_string()
    }
}

/// The most useful page the extension ships: its popup, else something close.
fn ext_page(dir: &Path, m: &Value) -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = m
        .pointer("/action/default_popup")
        .or_else(|| m.pointer("/browser_action/default_popup"))
        .and_then(|v| v.as_str())
    {
        candidates.push(p.trim_start_matches('/').to_string());
    }
    for p in [
        "popups/popup.html",
        "popup.html",
        "dashboard.html",
        "options.html",
        "index.html",
    ] {
        candidates.push(p.to_string());
    }
    candidates
        .into_iter()
        .find(|p| dir.join(p).is_file())
        .unwrap_or_default()
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("open", "open a URL or search"),
    ("open-tab", "open in a new tab"),
    ("tab-new", "new tab"),
    ("tab-close", "close current tab"),
    ("tab-next", "next tab"),
    ("tab-prev", "previous tab"),
    ("tab-undo", "reopen the last closed window"),
    ("extensions", "open an extension's own page"),
    ("q", "quit kite"),
    ("qall", "quit kite"),
    ("reload", "reload page"),
    ("reload-hard", "reload bypassing cache"),
    ("back", "go back"),
    ("forward", "go forward"),
    ("home", "go to start page"),
    ("zoom-in", "increase zoom"),
    ("zoom-out", "decrease zoom"),
    ("zoom-reset", "reset zoom"),
    ("images", "toggle images"),
    ("hint", "click a link by hint"),
    ("find", "find in page"),
    ("yank", "copy current URL"),
    ("yank-title", "copy current title"),
    ("bookmark", "toggle bookmark"),
    ("bookmarks", "list bookmarks"),
    ("history", "search history"),
    ("session-save", "save session"),
    ("private", "open a private tab"),
    ("external", "open in the system browser"),
    ("help", "show all keybindings"),
];

fn fuzzy(haystack: &str, needle: &str) -> bool {
    let mut it = haystack.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn clipboard_write(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (bin, args) in candidates {
        let Ok(mut child) = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            if stdin.write_all(text.as_bytes()).is_err() {
                continue;
            }
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn clipboard_read() -> Option<String> {
    use std::process::Command;
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn resolves_inputs() {
        let c = cfg();
        let home = "file:///home/x/.local/state/kite/home.html";
        assert_eq!(resolve_input(&c, "", home), home);
        assert_eq!(resolve_input(&c, "home", home), home);
        assert_eq!(
            resolve_input(&c, "127.0.0.1:8080", home),
            "http://127.0.0.1:8080"
        );
        assert_eq!(resolve_input(&c, "localhost:9", home), "http://localhost:9");
        assert_eq!(
            resolve_input(&c, "example.com", home),
            "https://example.com"
        );
        assert_eq!(resolve_input(&c, "lemonde.fr", home), "https://lemonde.fr");
        assert_eq!(
            resolve_input(&c, "example.com:8080/a?b", home),
            "https://example.com:8080/a?b"
        );
        assert_eq!(
            resolve_input(&c, "buildbox:8080", home),
            "https://buildbox:8080"
        );
        assert_eq!(
            resolve_input(&c, "3.14", home),
            "https://duckduckgo.com/?q=3.14",
            "a numeric dotted string is not a hostname"
        );
        assert_eq!(
            resolve_input(&c, "foo:bar", home),
            "https://duckduckgo.com/?q=foo%3Abar",
            "a non-numeric port is not a hostname"
        );
        assert_eq!(
            resolve_input(&c, "https://example.com", home),
            "https://example.com"
        );
        assert_eq!(resolve_input(&c, "about:blank", home), "about:blank");
        assert_eq!(resolve_input(&c, "javascript:alert(1)", home), home);
        assert_eq!(
            resolve_input(&c, "ddg two words", home),
            "https://duckduckgo.com/?q=two+words"
        );
        assert!(
            resolve_input(&c, "two words", home).starts_with("https://duckduckgo.com/?q=two+words")
        );
    }

    #[test]
    fn omnibox_preview_matches_navigation() {
        let c = cfg();
        let home = "file:///tmp/home.html";
        for q in [
            "lemonde.fr",
            "example.com:8080/a?b",
            "ddg two words",
            "wikipedia rust",
            "two words",
            "3.14",
            "foo:bar",
            "home",
            "javascript:alert(1)",
        ] {
            let t = interpret_input(&c, q, home);
            assert_eq!(t.url, resolve_input(&c, q, home), "input: {q}");
        }
        let t = interpret_input(&c, "lemonde.fr", home);
        assert_eq!(t.kind, InputKind::Url);
        assert_eq!(t.url, "https://lemonde.fr");
        let t = interpret_input(&c, "ddg two words", home);
        assert_eq!(t.kind, InputKind::Engine);
        assert_eq!(t.engine, "ddg");
        assert_eq!(t.query, "two words");
        let t = interpret_input(&c, "two words", home);
        assert_eq!(t.kind, InputKind::Search);
        assert_eq!(t.engine, "ddg");
    }

    #[test]
    fn extension_label_and_page_come_from_the_manifest() {
        let dir = std::env::temp_dir().join(format!("kite-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("popups")).unwrap();
        std::fs::create_dir_all(dir.join("_locales/en")).unwrap();
        std::fs::write(dir.join("popups/popup.html"), "<!doctype html>").unwrap();
        std::fs::write(
            dir.join("_locales/en/messages.json"),
            r#"{"extName":{"message":"Localised Name"}}"#,
        )
        .unwrap();
        let m: Value = serde_json::from_str(
            r#"{"name":"__MSG_extName__","default_locale":"en",
               "action":{"default_popup":"popups/popup.html"},
               "background":{"service_worker":"bg.js"}}"#,
        )
        .unwrap();
        assert_eq!(ext_name(&dir, &m), "Localised Name");
        assert_eq!(ext_page(&dir, &m), "popups/popup.html");
        assert_eq!(background_path(&m), "bg.js");
        let slashed: Value =
            serde_json::from_str(r#"{"background":{"service_worker":"/js/background.js"}}"#)
                .unwrap();
        assert_eq!(background_path(&slashed), "js/background.js");

        // A popup the package does not ship is not offered.
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(ext_page(&empty, &m), "");
        // A literal name is used as-is; no manifest falls back to the dir name.
        let literal: Value = serde_json::from_str(r#"{"name":"Plain"}"#).unwrap();
        assert_eq!(ext_name(&empty, &literal), "Plain");
        assert_eq!(dir_label(&empty), "empty");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_page_is_the_built_in_home() {
        let mut c = cfg();
        c.general
            .insert("start_page".into(), serde_json::json!("home"));
        let home = "file:///tmp/home.html";
        assert_eq!(resolve_input(&c, "", home), home);
        c.general
            .insert("start_page".into(), serde_json::json!("about:blank"));
        assert_eq!(resolve_input(&c, "", home), home);
        c.general.insert(
            "start_page".into(),
            serde_json::json!("https://example.com"),
        );
        assert_eq!(resolve_input(&c, "", home), "https://example.com");
    }
}
