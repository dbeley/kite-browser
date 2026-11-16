//! The browser shell: owns tabs (one Chromium target each), navigation,
//! history/bookmarks/session, and the privileged side of the in-page command
//! surface. Everything here is invoked from the UI layer over a CDP binding.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cdp::{Cdp, Event};
use crate::config::Config;
use crate::engine::Engine;
use crate::store::{SessionTab, Store};

pub struct Tab {
    pub id: String,
    pub session: String,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub private: bool,
}

struct Closed {
    url: String,
    title: String,
}

pub struct Shell {
    pub cdp: Cdp,
    pub engine: Engine,
    pub cfg: Config,
    pub store: Store,
    pub tabs: Vec<Tab>,
    pub active: usize,
    closed: Vec<Closed>,
    ui: String,
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
        boot_url: String,
        restore: Vec<String>,
    ) -> Result<Shell, String> {
        let startup_mark = Instant::now();
        cdp.call(
            None,
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
        )?;

        let mut shell = Shell {
            cdp,
            engine,
            cfg,
            store,
            tabs: Vec::new(),
            active: 0,
            closed: Vec::new(),
            ui,
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

    /// Enable the domains we need, register the command binding and install the
    /// UI script into every future document of this target.
    fn prepare(&mut self, idx: usize) -> Result<(), String> {
        let session = self.tabs[idx].session.clone();
        let _ = self.cdp.call(Some(&session), "Page.enable", json!({}));
        let _ = self.cdp.call(Some(&session), "Runtime.enable", json!({}));
        let _ = self.cdp.call(Some(&session), "Log.enable", json!({}));
        if crate::engine::env_flag("KITE_NO_UI") {
            return Ok(());
        }
        let _ = self.cdp.call(
            Some(&session),
            "Runtime.addBinding",
            json!({ "name": "__kiteSend" }),
        );
        let _ = self.cdp.call(
            Some(&session),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": self.ui, "runImmediately": true }),
        );
        // The current document predates the injection.
        self.inject_now(idx);
        Ok(())
    }

    fn inject_now(&mut self, idx: usize) {
        let session = self.tabs[idx].session.clone();
        let _ = self.cdp.eval(Some(&session), &self.ui);
        let boot = self.boot_payload(idx);
        let push = format!(
            "(()=>{{try{{window.__kite&&window.__kite.boot({})}}catch(e){{}}}})()",
            boot
        );
        let _ = self.cdp.eval(Some(&session), &push);
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
        let _ = self.cdp.eval(Some(&session), &expr);
    }

    pub fn toast(&mut self, msg: &str) {
        let Some(session) = self.active_session() else {
            return;
        };
        let expr = format!(
            "(()=>{{try{{window.__kite&&window.__kite.toast({})}}catch(e){{}}}})()",
            json!(msg)
        );
        let _ = self.cdp.eval(Some(&session), &expr);
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
        self.closed.push(Closed {
            url: tab.url.clone(),
            title: tab.title.clone(),
        });
        if self.closed.len() > 20 {
            self.closed.remove(0);
        }
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

    fn remember_visit(&mut self) {
        if let Some(t) = self.tabs.get(self.active) {
            let (u, title) = (t.url.clone(), t.title.clone());
            self.store.visit(&u, &title);
        }
    }

    /// "example.com" -> https URL, "ddg foo" -> engine query, "rust async" -> search.
    pub fn resolve_url(&self, input: &str) -> String {
        let s = input.trim();
        if s.is_empty() {
            return self.start_page();
        }
        if s.starts_with("javascript:") {
            return self.start_page();
        }
        let has_scheme = s.contains("://")
            || s.starts_with("about:")
            || s.starts_with("file:")
            || s.starts_with("data:")
            || s.starts_with("view-source:");
        if has_scheme {
            return s.to_string();
        }
        if s.starts_with("localhost") || s.starts_with("127.0.0.1") || s.starts_with("[::1]") {
            return format!("http://{s}");
        }
        // "engine prefix query"
        if let Some((head, rest)) = s.split_once(' ') {
            if let Some(engine) = self.cfg.search.get(head).and_then(|v| v.as_str()) {
                return engine.replace("{}", &urlencode(rest));
            }
        }
        let hostish = !s.contains(' ') && (s.contains('.') || s.contains(':')) && !s.ends_with('.');
        if hostish {
            return format!("https://{s}");
        }
        let engine_name = self.cfg.get_str("search_engine").unwrap_or("ddg");
        let engine = self
            .cfg
            .search
            .get(engine_name)
            .and_then(|v| v.as_str())
            .unwrap_or("https://duckduckgo.com/?q={}");
        engine.replace("{}", &urlencode(s))
    }

    pub fn start_page(&self) -> String {
        self.cfg
            .get_str("start_page")
            .unwrap_or("about:blank")
            .to_string()
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
            if self.quit {
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
                    if let Some(t) = self.tab_by_session(&ev.session) {
                        let idx = t;
                        self.tabs[idx].url = url;
                        self.tabs[idx].loading = true;
                        if self.active == idx {
                            self.push_state();
                        }
                        self.session_dirty = true;
                    }
                }
            }
            "Page.loadEventFired" => {
                if let Some(idx) = self.tab_by_session(&ev.session) {
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
                    // Engine-side close (page crash, :q from a page, etc.)
                    if self.tabs.len() == 1 {
                        self.quit = true;
                        return;
                    }
                    self.tabs.remove(idx);
                    if self.active >= self.tabs.len() {
                        self.active = self.tabs.len() - 1;
                    }
                    self.session_dirty = true;
                    self.push_state();
                }
            }
            "Page.javascriptDialogOpening" => {
                let _ = self.cdp.call(
                    ev.session.as_deref(),
                    "Page.handleJavaScriptDialog",
                    json!({ "accept": true }),
                );
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
        let expr = format!(
            "(()=>{{try{{window.__kite&&window.__kite.recv({}, {})}}catch(e){{}}}})()",
            rid, body
        );
        let _ = self.cdp.eval(Some(&s), &expr);
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
                let _ = self.cdp.eval(Some(&s), &expr);
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
                    let engine = self.cfg.get_str("search_engine").unwrap_or("ddg");
                    out.push(json!({
                        "text": format!("{engine}: {q}"),
                        "detail": "search",
                        "kind": "search",
                    }));
                }
            }
        }
        out.truncate(limit);
        out
    }
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("open", "open a URL or search"),
    ("open-tab", "open in a new tab"),
    ("tab-new", "new tab"),
    ("tab-close", "close current tab"),
    ("tab-next", "next tab"),
    ("tab-prev", "previous tab"),
    ("tab-undo", "reopen last closed tab"),
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
