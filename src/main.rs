//! kite — a keyboard-first browser shell on the Chromium engine.
//!
//! Design in one line: Chromium is the engine, kite is a ~10 MB controller and
//! a native-speed keyboard UI layer, with zero Python and zero Qt in the path.

mod cdp;
mod config;
mod engine;
mod shell;
mod store;
mod ws;

use std::time::Instant;

use config::Config;
use engine::{Engine, EngineOptions};
use shell::Shell;
use store::Store;

const UI_JS: &str = include_str!("ui.js");
/// Must match `VERSION` at the top of ui.js — the selftest asserts the injected
/// layer is actually the one compiled into this binary.
const UI_VERSION: &str = "0.2.0";

struct Args {
    config: Option<std::path::PathBuf>,
    state: Option<std::path::PathBuf>,
    port: u16,
    engine_bin: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    incognito: bool,
    no_restore: bool,
    selftest: bool,
    bench: bool,
    url: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "kite — keyboard-first browser on the Chromium engine

usage: kite [options] [url]

options:
  -c, --config PATH    config file (default $XDG_CONFIG_HOME/kite/config.toml)
      --state DIR      state dir for history/bookmarks/session/profile
      --engine BIN     chromium binary (default: auto-detect)
      --port N         fixed DevTools port (default: random)
      --width N        window width
      --height N       window height
  -i, --incognito      start with a private profile
      --no-restore     do not restore the previous session
      --selftest       launch, verify the UI layer, exit
      --bench          measure cold start to first paint, exit
  -h, --help           this text
  -V, --version        version
"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        config: None,
        state: None,
        port: 0,
        engine_bin: None,
        width: None,
        height: None,
        incognito: false,
        no_restore: false,
        selftest: false,
        bench: false,
        url: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> String {
            it.next().unwrap_or_else(|| {
                eprintln!("kite: {name} needs a value");
                std::process::exit(2)
            })
        };
        match arg.as_str() {
            "-h" | "--help" => usage(),
            "-V" | "--version" => {
                println!("kite {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-c" | "--config" => a.config = Some(take(&arg).into()),
            "--state" => a.state = Some(take(&arg).into()),
            "--engine" => a.engine_bin = Some(take(&arg)),
            "--port" => a.port = take(&arg).parse().unwrap_or(0),
            "--width" => a.width = take(&arg).parse().ok(),
            "--height" => a.height = take(&arg).parse().ok(),
            "-i" | "--incognito" => a.incognito = true,
            "--no-restore" => a.no_restore = true,
            "--selftest" => a.selftest = true,
            "--bench" => a.bench = true,
            other if other.starts_with('-') && other.len() > 1 => {
                eprintln!("kite: unknown option {other}");
                usage();
            }
            other => a.url = Some(other.to_string()),
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let started = Instant::now();

    let cfg_path = args.config.clone().unwrap_or_else(config::config_path);
    let system_cfg = if args.config.is_none() { config::system_config_path() } else { None };
    if !cfg_path.exists() && system_cfg.is_none() {
        if let Some(parent) = cfg_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Ship the documented defaults so the file is discoverable and editable.
        let _ = std::fs::write(&cfg_path, config::DEFAULT_CONFIG.trim_start());
        eprintln!("kite: wrote default config to {}", cfg_path.display());
    }
    let cfg = match Config::load(system_cfg.as_deref(), &cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kite: {e}");
            std::process::exit(1);
        }
    };

    let state_dir = args
        .state
        .clone()
        .or_else(|| std::env::var("KITE_HOME").ok().map(std::path::PathBuf::from))
        .unwrap_or_else(config::state_dir);
    let mut store = Store::load(state_dir.clone());

    let restore_enabled = cfg.get_bool("restore_session").unwrap_or(true) && !args.no_restore;
    let mut restore: Vec<String> = Vec::new();
    let mut boot_url = args.url.clone();
    if boot_url.is_none() && restore_enabled {
        if let Some(s) = store.session.as_ref() {
            let mut urls: Vec<String> = s
                .tabs
                .iter()
                .map(|t| t.url.clone())
                .filter(|u| !u.is_empty() && u != "about:blank")
                .collect();
            if !urls.is_empty() {
                boot_url = Some(urls.remove(0));
                restore = urls;
            }
        }
    }
    let boot_url = boot_url.unwrap_or_else(|| {
        cfg.get_str("start_page").unwrap_or("about:blank").to_string()
    });

    if args.incognito && !args.state.is_some() {
        // Isolate the engine profile for the run, keep the config.
        store = Store::load(std::env::temp_dir().join(format!("kite-incognito-{}", std::process::id())));
    }

    let profile = state_dir.join("engine-profile");
    let engine_opts = EngineOptions {
        binary: args
            .engine_bin
            .clone()
            .or_else(|| cfg.get_str("engine").map(str::to_string))
            .unwrap_or_else(|| "chromium".to_string()),
        profile,
        url: boot_url.clone(),
        port: args.port,
        extra_args: cfg.extra_args(),
        width: args.width.or(cfg.get_i64("width").map(|v| v as u32)).unwrap_or(1280),
        height: args.height.or(cfg.get_i64("height").map(|v| v as u32)).unwrap_or(820),
        incognito: args.incognito,
    };

    let engine = match Engine::launch(&engine_opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("kite: {e}");
            eprintln!("kite: install chromium, or pass --engine /path/to/chromium");
            std::process::exit(1);
        }
    };
    let devtools_port = engine.port;
    let launch_ms = started.elapsed().as_millis();

    let cdp = match cdp::Cdp::connect(&engine.ws_url) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kite: {e}");
            eprintln!("kite: debug endpoint was {}", engine.ws_url);
            std::process::exit(1);
        }
    };

    let mut shell = match Shell::new(
        cdp,
        engine,
        cfg,
        store,
        UI_JS.to_string(),
        boot_url.clone(),
        if args.incognito { Vec::new() } else { restore },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kite: {e}");
            std::process::exit(1);
        }
    };

    let first_paint = shell.wait_first_paint(args.selftest || args.bench);
    if args.selftest {
        selftest(&mut shell, launch_ms, first_paint);
        return;
    }

    if std::env::var("KITE_TIMING").is_ok() {
        eprintln!(
            "kite: engine ready in {launch_ms} ms (devtools port {}), first paint {} ms",
            devtools_port,
            first_paint.map(|d| d.as_millis().to_string()).unwrap_or_else(|| "?".into())
        );
    }

    if args.bench {
        let state = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ps -o rss= --ppid $(pgrep -f 'kite' | head -1) 2>/dev/null | head -1"
            ))
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        println!(
            "{{\"launch_ms\":{},\"first_paint_ms\":{},\"ui_rss_kb\":{},\"pid\":{}}}",
            launch_ms,
            first_paint.map(|d| d.as_millis()).unwrap_or(0),
            if state.is_empty() { "0".to_string() } else { state },
            std::process::id()
        );
        return;
    }

    if let Err(e) = shell.run() {
        eprintln!("kite: {e}");
        std::process::exit(1);
    }
}

/// (key, code, virtual key code, text) for the keys the selftest drives.
fn key_spec(k: &str) -> (String, String, i64, String) {
    let (code, vk, text) = match k {
        "Escape" => ("Escape", 27, ""),
        "Enter" => ("Enter", 13, "\r"),
        "Tab" => ("Tab", 9, ""),
        "Backspace" => ("Backspace", 8, ""),
        ":" => ("Semicolon", 186, ":"),
        "/" => ("Slash", 191, "/"),
        "?" => ("Slash", 191, "?"),
        " " => ("Space", 32, " "),
        "g" => ("KeyG", 71, "g"),
        "G" => ("KeyG", 71, "G"),
        "," => ("Comma", 188, ","),
        "." => ("Period", 190, "."),
        "-" => ("Minus", 189, "-"),
        other => {
            let ch = other.chars().next().unwrap_or(' ');
            let up = ch.to_ascii_uppercase();
            return (
                other.to_string(),
                format!("Key{up}"),
                up as i64,
                if ch.is_ascii_uppercase() { other.to_string() } else { ch.to_string() },
            );
        }
    };
    (k.to_string(), code.to_string(), vk, text.to_string())
}

fn selftest(shell: &mut Shell, launch_ms: u128, first_paint: Option<std::time::Duration>) {
    // Give the renderer a beat: on a 2-core box the compositor is still coming
    // up when the load event lands.
    let wait_ms: u64 = std::env::var("KITE_SELFTEST_WAIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    shell.pump(std::time::Duration::from_millis(wait_ms));
    let session = shell.tabs.first().map(|t| t.session.clone());
    let probe = shell
        .cdp
        .eval(session.as_deref(), "typeof window.__kite === 'object' ? window.__kite.version : 'missing'");
    let dom = shell.cdp.eval(
        session.as_deref(),
        "JSON.stringify({title: document.title, href: location.href, text: (document.body ? document.body.innerText : '').slice(0,60), kids: document.body ? document.body.children.length : -1, kite: !!window.__kite, state: (window.__kite && window.__kite._state) ? window.__kite._state() : null})",
    );
    let cfg = shell.cdp.eval(
        session.as_deref(),
        "(window.__kite && window.__kite._cfg) ? JSON.stringify(window.__kite._cfg()) : 'no hook'",
    );
    let shot = shell
        .cdp
        .call(session.as_deref(), "Page.captureScreenshot", serde_json::json!({ "format": "png" }));
    match shot {
        Ok(v) => {
            if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                use base64::Engine as _;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                    let path = "/tmp/kite-selftest.png";
                    let _ = std::fs::write(path, &bytes);
                    println!("  screenshot   : {path} ({} bytes)", bytes.len());
                }
            } else {
                println!("  screenshot   : no data field: {v}");
            }
        }
        Err(e) => println!("  screenshot   : failed: {e}"),
    }
    println!("selftest:");
    println!("  engine ready : {launch_ms} ms");
    println!(
        "  first paint  : {} ms",
        first_paint.map(|d| d.as_millis().to_string()).unwrap_or_else(|| "n/a".into())
    );
    println!("  ui layer     : {probe:?}");
    println!("  dom          : {}", dom.map(|v| v.to_string()).unwrap_or_else(|e| format!("error: {e}")));
    println!("  keymap       : {}", cfg.map(|v| v.to_string()).unwrap_or_else(|e| format!("error: {e}")));
    if let Ok(expr) = std::env::var("KITE_EVAL") {
        let r = shell.cdp.eval(session.as_deref(), &expr);
        println!("  eval         : {}", r.map(|v| v.to_string()).unwrap_or_else(|e| format!("error: {e}")));
    }
    // Real key path: CDP input events go through the browser's own pipeline,
    // exactly like an X11 keystroke. Used to test the keymap end to end.
    if let Ok(keys) = std::env::var("KITE_KEYS") {
        for key in keys.split_whitespace() {
            let spec = key_spec(key);
            let params = serde_json::json!({
                "type": "keyDown",
                "key": spec.0,
                "code": spec.1,
                "windowsVirtualKeyCode": spec.2,
                "nativeVirtualKeyCode": spec.2,
                "text": spec.3,
            });
            if let Err(e) = shell.cdp.call(session.as_deref(), "Input.dispatchKeyEvent", params) {
                println!("  keys         : {key} failed: {e}");
            }
            let up = serde_json::json!({
                "type": "keyUp",
                "key": spec.0,
                "code": spec.1,
                "windowsVirtualKeyCode": spec.2,
            });
            let _ = shell.cdp.call(session.as_deref(), "Input.dispatchKeyEvent", up);
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        // Commands travel JS -> binding -> controller, so events must be pumped
        // before reading state back.
        shell.pump(std::time::Duration::from_millis(800));
        let after = shell.cdp.eval(
            session.as_deref(),
            "JSON.stringify({mode:(window.__kite&&window.__kite._state)?window.__kite._state().mode:'?', err:(window.__kite&&window.__kite._state)?window.__kite._state().err:null, key:(window.__kite&&window.__kite._state)?window.__kite._state().key:null, hints:document.querySelectorAll('.h').length, palette:(window.__kite&&window.__kite._palette)?window.__kite._palette():null, scrollY:Math.round(window.scrollY)})",
        );
        println!("  keys         : {keys} -> {}", after.map(|v| v.to_string()).unwrap_or_else(|e| format!("error: {e}")));
    }
    let tabs = shell.tabs.len();
    println!("  tabs         : {tabs}");
    println!("  active url   : {}", shell.tabs.first().map(|t| t.url.clone()).unwrap_or_default());
    let ok = matches!(probe, Ok(serde_json::Value::String(ref s)) if s == UI_VERSION);
    println!("  result       : {}", if ok { "OK" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}
