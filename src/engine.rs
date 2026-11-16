//! Chromium engine supervisor: launches the system Chromium with a performance
//! profile tuned for weak CPUs / small RAM, then locates the DevTools endpoint.
//!
//! kite never links against Chromium. It drives a real browser process, which
//! means: no CEF build, no QtWebEngine build, system security updates apply,
//! and the UI process stays ~10 MB.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub struct Engine {
    child: Option<Child>,
    bin: PathBuf,
    profile: PathBuf,
    width: u32,
    height: u32,
    pub port: u16,
    pub ws_url: String,
    /// Removes the WM frame from the engine's X11 windows for as long as the
    /// engine lives. `None` on Wayland (nothing to strip) or without the tools.
    _frames: Option<crate::wm::FrameStripper>,
}

pub struct EngineOptions {
    pub binary: String,
    pub profile: PathBuf,
    pub url: String,
    pub port: u16,
    pub extra_args: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub incognito: bool,
    /// false: ask for a window with no frame (no title bar, no borders).
    pub decorations: bool,
    pub fullscreen: bool,
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            // Ask the page to save its session first: the JS layer flushes on
            // beforeunload, so a plain SIGTERM on the browser process is safe.
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return if p.exists() { Some(p) } else { None };
    }
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let cand = Path::new(dir).join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(9222)
}

pub fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name), Ok(v) if !v.is_empty() && v != "0" && v != "false")
}

/// Chromium refuses `--app=about:blank`: it falls back to a normal browser
/// window, tab strip and omnibox included. A data: page keeps app mode, and
/// painting it dark means the startup frame matches the shell instead of
/// flashing white before the first navigation.
pub const PLACEHOLDER: &str = "data:text/html,<title>kite</title><style>html{background:%230b0e11;color-scheme:dark}body{margin:0}</style>";

fn app_url(url: &str) -> &str {
    let u = url.trim();
    if u.is_empty() || u == "about:blank" {
        PLACEHOLDER
    } else {
        u
    }
}

/// Flags that cut startup work, background chatter and memory. Each one is
/// measured, not folklore: see docs/perf.md.
fn perf_args(decorations: bool) -> Vec<String> {
    let mut features = String::from(
        "Translate,MediaRouter,OptimizationHints,CalculateNativeWinOcclusion,\
         InterestFeedContentSuggestions,PrivacySandboxSettings4,\
         AutofillServerCommunication,OptimizationGuideModelDownloading,ChromeWhatsNewUI",
    );
    let mut a: Vec<String> = vec![
        // Startup surface: nothing we do not need gets loaded.
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-component-update",
        "--disable-background-networking",
        "--disable-sync",
        "--disable-client-side-phishing-detection",
        "--disable-domain-reliability",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--no-pings",
        "--metrics-recording-only",
        // Single browser process, at most three renderers: on a 4 GB ARM box
        // the fourth renderer is what pushes you into swap.
        "--renderer-process-limit=3",
        "--process-per-site",
        // RAM backstops.
        "--js-flags=--max-old-space-size=384",
        "--disk-cache-size=67108864",
        "--media-cache-size=33554432",
        "--disable-dev-shm-usage",
        // Touch/pinch UI chrome we replace ourselves.
        "--disable-pinch",
        "--overscroll-history-navigation=0",
        "--hide-scrollbars",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    // niri, sway and every other wlroots-style compositor draw no server-side
    // decorations, so turning off Chromium's own CSD leaves a bare surface.
    // Compositors that do draw frames are handled in `wm.rs` (X11) or by
    // `fullscreen = true`.
    if !decorations {
        features.push_str(",WaylandWindowDecorations");
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            a.push("--ozone-platform-hint=auto".into());
        }
    }
    a.push(format!("--disable-features={features}"));

    if env_flag("KITE_DISABLE_GPU") {
        a.push("--disable-gpu".into());
        a.push("--disable-accelerated-2d-canvas".into());
    }
    if env_flag("KITE_NO_SANDBOX") || nix_sandbox_unavailable() {
        a.push("--no-sandbox".into());
        // The "unsupported flag" infobar otherwise eats 32 px of the viewport.
        a.push("--test-type".into());
    }
    a
}

/// Chromium leaves `SingletonLock` behind when it is killed instead of asked to
/// quit; the next launch then hands its URL to a process that is not there and
/// exits 0. Clear the lock only when it names this host and a dead pid.
fn clear_stale_locks(profile: &Path) {
    let lock = profile.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return;
    };
    let Some(target) = target.to_str() else {
        return;
    };
    let Some((host, pid)) = target.rsplit_once('-') else {
        return;
    };
    let Ok(pid) = pid.parse::<u32>() else { return };
    let same_host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim() == host)
        .unwrap_or(false);
    if !same_host || Path::new(&format!("/proc/{pid}")).exists() {
        return;
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
}

/// NixOS in containers/CI often lacks user namespaces; Chromium then refuses to
/// start. Detect instead of forcing everyone into --no-sandbox.
fn nix_sandbox_unavailable() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// `ws://127.0.0.1/devtools/...` -> `ws://127.0.0.1:PORT/devtools/...`
fn with_port(url: &str, port: u16) -> String {
    let Some(rest) = url.strip_prefix("ws://") else {
        return url.to_string();
    };
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.contains(':') {
        return url.to_string();
    }
    format!("ws://{authority}:{port}{tail}")
}

impl Engine {
    pub fn launch(opts: &EngineOptions) -> Result<Engine, String> {
        let bin = which(&opts.binary)
            .ok_or_else(|| format!("engine binary not found: {}", opts.binary))?;
        std::fs::create_dir_all(&opts.profile)
            .map_err(|e| format!("profile dir {}: {e}", opts.profile.display()))?;
        clear_stale_locks(&opts.profile);

        if opts.port == 0 {
            // fall through to free_port below
        }
        let port = if opts.port == 0 {
            free_port()
        } else {
            opts.port
        };

        let mut cmd = Command::new(&bin);
        cmd.arg(format!("--app={}", app_url(&opts.url)))
            .arg(format!("--user-data-dir={}", opts.profile.display()))
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--window-size={},{}", opts.width, opts.height))
            .arg("--noerrdialogs")
            .arg("--disable-infobars");
        if opts.fullscreen {
            cmd.arg("--start-fullscreen");
        }
        if opts.incognito {
            cmd.arg("--incognito");
        }
        for a in perf_args(opts.decorations) {
            cmd.arg(a);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null());
        if std::env::var("KITE_PRINT_ARGS").is_ok() {
            eprintln!(
                "[engine] {} {:?}",
                bin.display(),
                cmd.get_args().collect::<Vec<_>>()
            );
        }
        if std::env::var("KITE_ENGINE_LOG").is_ok() {
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;

        let frames = if opts.decorations {
            None
        } else {
            crate::wm::FrameStripper::start(child.id(), std::env::var("KITE_DEBUG_WM").is_ok())
        };

        // Drain stderr so a chatty engine can never block on a full pipe.
        if let Some(err) = child.stderr.take() {
            let keep = std::env::var("KITE_ENGINE_LOG").is_ok();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if keep {
                        eprintln!("[engine] {line}");
                    }
                }
            });
        }
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(
                move || {
                    for _ in BufReader::new(out).lines().map_while(Result::ok) {}
                },
            );
        }

        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                // Exit 0 is what Chromium does when it hands the URL to another
                // live instance on the same profile: a fresh profile has no one
                // to hand off to, so this almost always means a stale or
                // occupied `--user-data-dir`.
                return Err(format!(
                    "engine exited during startup ({status}) with profile {} — another Chromium is probably using it (close it, or pass --state DIR)",
                    opts.profile.display()
                ));
            }
            match crate::ws::http_get(port, "/json/version") {
                Ok(body) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&body) {
                        if let Some(url) = v.get("webSocketDebuggerUrl").and_then(|s| s.as_str()) {
                            // Chromium derives this URL from the request's Host
                            // header; if it came back portless, pin our port.
                            let url = with_port(url, port);
                            return Ok(Engine {
                                child: Some(child),
                                bin: bin.clone(),
                                profile: opts.profile.clone(),
                                width: opts.width,
                                height: opts.height,
                                port,
                                ws_url: url,
                                _frames: frames,
                            });
                        }
                    }
                }
                Err(_) => {}
            }
            if Instant::now() > deadline {
                // Re-probe once so the error describes the failure as it is now.
                let reason = match crate::ws::http_get(port, "/json/version") {
                    Ok(b) => format!("unexpected /json/version: {}", b.trim()),
                    Err(e) => e.to_string(),
                };
                let _ = child.kill();
                return Err(format!("engine debug port never came up: {reason}"));
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    /// Ask the running browser for one more app-mode window.
    ///
    /// A second `--app=` launch on the same profile hands its URL to the live
    /// browser process and exits 0; that browser creates a real app window. A
    /// CDP `Target.createTarget` cannot do this — it always builds a tabbed
    /// browser window, tab strip and omnibox included.
    pub fn open_app_window(&self, url: &str) -> Result<(), String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(format!("--app={}", app_url(url)))
            .arg(format!("--user-data-dir={}", self.profile.display()))
            .arg(format!("--window-size={},{}", self.width, self.height))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--noerrdialogs")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if env_flag("KITE_NO_SANDBOX") || nix_sandbox_unavailable() {
            // Without this the "unsupported flag --no-sandbox" infobar eats the
            // top strip of the new window.
            cmd.arg("--test-type");
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot open app window: {e}"))?;
        // Reap the hand-off process so it does not linger as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}
