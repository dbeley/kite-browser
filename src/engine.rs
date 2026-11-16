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
    pub port: u16,
    pub ws_url: String,
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

/// Flags that cut startup work, background chatter and memory. Each one is
/// measured, not folklore: see docs/perf.md.
fn perf_args() -> Vec<String> {
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
        "--disable-features=Translate,MediaRouter,OptimizationHints,CalculateNativeWinOcclusion,InterestFeedContentSuggestions,PrivacySandboxSettings4,AutofillServerCommunication,OptimizationGuideModelDownloading,ChromeWhatsNewUI",
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

/// NixOS in containers/CI often lacks user namespaces; Chromium then refuses to
/// start. Detect instead of forcing everyone into --no-sandbox.
fn nix_sandbox_unavailable() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// `ws://127.0.0.1/devtools/...` -> `ws://127.0.0.1:PORT/devtools/...`
fn with_port(url: &str, port: u16) -> String {
    let Some(rest) = url.strip_prefix("ws://") else { return url.to_string() };
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

        if opts.port == 0 {
            // fall through to free_port below
        }
        let port = if opts.port == 0 { free_port() } else { opts.port };

        let mut cmd = Command::new(&bin);
        cmd.arg(format!("--app={}", opts.url))
            .arg(format!("--user-data-dir={}", opts.profile.display()))
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--window-size={},{}", opts.width, opts.height))
            .arg("--noerrdialogs")
            .arg("--disable-infobars");
        if opts.incognito {
            cmd.arg("--incognito");
        }
        for a in perf_args() {
            cmd.arg(a);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null());
        if std::env::var("KITE_PRINT_ARGS").is_ok() {
            eprintln!("[engine] {} {:?}", bin.display(), cmd.get_args().collect::<Vec<_>>());
        }
        if std::env::var("KITE_ENGINE_LOG").is_ok() {
            cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;

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
            std::thread::spawn(move || {
                for _ in BufReader::new(out).lines().map_while(Result::ok) {}
            });
        }

        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("engine exited during startup ({status})"));
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
                                port,
                                ws_url: url,
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

    pub fn alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}
