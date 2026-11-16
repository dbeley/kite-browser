//! Strip the window frame from the engine's windows on X11.
//!
//! Chromium has no "undecorated window" flag. On Wayland that does not matter:
//! with `WaylandWindowDecorations` disabled Chromium draws no client-side frame,
//! and compositors like niri and sway draw no server-side one either. On X11
//! the window manager draws the frame, and the only portable way to opt out is
//! the Motif hint `_MOTIF_WM_HINTS` — honoured by i3, openbox, xfwm, kwin,
//! awesome and most others (GNOME ignores it; use `fullscreen` there).
//!
//! Every tab is its own engine window, so new windows appear at arbitrary times.
//! A small poller watches the engine's pid and decorates each window once.

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct FrameStripper {
    stop: Arc<AtomicBool>,
}

impl FrameStripper {
    /// `None` when there is nothing to do (Wayland, no X display) or the X
    /// tooling is missing. Never fatal: a decorated window still works.
    pub fn start(pid: u32, debug: bool) -> Option<FrameStripper> {
        if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_err() {
            return None;
        }
        if !have("xdotool") || !have("xprop") {
            if debug {
                eprintln!("[wm] xdotool or xprop missing: leaving the window frame alone");
            }
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("kite-frames".into())
            .spawn(move || poll(pid, flag, debug))
            .ok()?;
        Some(FrameStripper { stop })
    }
}

impl Drop for FrameStripper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn poll(pid: u32, stop: Arc<AtomicBool>, debug: bool) {
    let mut seen: HashSet<String> = HashSet::new();
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        for id in windows_of(pid) {
            if !seen.insert(id.clone()) {
                continue;
            }
            if debug {
                eprintln!("[wm] stripping frame from window {id}");
            }
            let _ = Command::new("xprop")
                .args([
                    "-id",
                    &id,
                    "-f",
                    "_MOTIF_WM_HINTS",
                    "32c",
                    "-set",
                    "_MOTIF_WM_HINTS",
                    "0x2, 0x0, 0x0, 0x0, 0x0",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // Windows show up as tabs are opened. Poll fast for the first seconds,
        // then often enough that a new tab loses its frame quickly, cheaply
        // enough for a two-core box.
        let nap = if started.elapsed() < Duration::from_secs(10) {
            Duration::from_millis(250)
        } else {
            Duration::from_millis(1500)
        };
        std::thread::sleep(nap);
    }
}

fn windows_of(pid: u32) -> Vec<String> {
    let out = Command::new("xdotool")
        .args(["search", "--pid", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}
