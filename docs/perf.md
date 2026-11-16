# Performance: what kite does, what it can't do, and how to check

The premise is narrow: **the browser engine is not the thing to optimise**. On a
2-core ARM board, Chromium's renderer will cost what it costs. What a browser
shell controls is everything *around* the engine — and that is where qutebrowser
pays: a Python interpreter, a Qt application object, and QtWebEngine's wrapper,
all resident before a single page is parsed.

kite moves that layer out of the way:

| Layer | qutebrowser | kite |
| --- | --- | --- |
| Shell language | Python (CPython + ~1 MB of .pyc) | Rust, 857 KB static binary |
| UI toolkit | Qt widgets + QtWebEngine | none in the shell process |
| Key handling | Qt event → Python → command registry | Chromium key path → one JS handler |
| Engine | QtWebEngine (Chromium, embedded) | system Chromium (separate process) |
| Tab bookkeeping | in-process Python objects | CDP target IDs in the Rust shell |

The measurable consequence: kite's own process stays in single-digit megabytes
regardless of page count, and keystrokes never cross a process boundary. What
kite cannot change is the cost of the renderer itself, which is the same
Chromium either way.

## Engine flags

Every flag in `engine.rs::perf_args` is there for a reason. The categories:

**Startup surface.** `--disable-extensions`, `--disable-default-apps`,
`--disable-component-update`, `--no-first-run`, `--no-default-browser-check`,
`--disable-sync`. Extensions alone can add a renderer process per extension.

**Background chatter.** `--disable-background-networking`,
`--disable-domain-reliability`, `--disable-client-side-phishing-detection`,
`--no-pings`, `--metrics-recording-only`, plus `--disable-features=Translate,
MediaRouter,OptimizationHints,...`. These keep the CPU quiet between page loads,
which matters more than startup on battery-powered ARM.

**Process count and heap.**
- `--renderer-process-limit=3` — the fourth renderer is usually what pushes a
  4 GB device into swap.
- `--process-per-site` — collapses same-site tabs into one renderer.
- `--js-flags=--max-old-space-size=384` — V8 stops growing before the machine
  does. Trades a tab crash on pathological pages for a usable system.

**Cache ceilings.** `--disk-cache-size=67108864`, `--media-cache-size=33554432`.
Default cache sizes assume a desktop SSD.

**Memory overcommit.** `--disable-dev-shm-usage` — on small containers and some
ARM images `/dev/shm` is 64 MB, and renderers crash with it full.

**Chrome we replace.** `--disable-pinch`, `--overscroll-history-navigation=0`,
`--hide-scrollbars`, `--noerrdialogs`, `--disable-infobars`.

Do **not** add `--disable-gpu` on a machine with any GPU: on several builds it
leaves the page unpainted under software compositing. `KITE_DISABLE_GPU=1` exists
for genuinely GPU-less environments, and it is off by default for that reason
(we hit exactly this failure while testing).

## Measuring

`dev/bench.sh` runs three cold starts under a real X server and reports:

- **window_s** — wall time from `exec` to a mapped, named window.
- **ui_rss_mb** — RSS of the shell process alone.
- **pss_mb** — sum of `Pss` across the whole process tree. This is the number to
  quote; summed RSS double-counts every shared library and shared-memory segment.
- **processes** — how many processes the browser really is.

```sh
nix develop
./dev/bench.sh /tmp/bench.jsonl
```

`kite --bench` reports the internal view instead: engine ready, first paint
(measured to `Page.loadEventFired` of the first real document), and its own RSS.

## What to expect on weak hardware

- Cold start is dominated by Chromium's startup, not by kite. Measured on this
  4-core test box (Xvfb, software raster, Nix store binary): **1.35 s** from exec
  to a mapped window, engine debug port up at ~1.5 s, first paint ~1.0 s after
  that. The shell's own work before the engine is up is a config read and a
  socket; everything else is Chromium finding its profile and creating a window.
- Shell memory is flat: **11 MB** RSS (9.5 MB PSS), 5 threads, regardless of tab
  count. For comparison, qutebrowser's shell process measured 139.9 MB on an
  earlier 2-core box in the same way — the ratio is the point, not the digits.
- The figures previously written here (0.37 s, 3.2 MB) came from a smaller,
  older machine and are not comparable with the ones above; they are kept only
  in this note so nobody mixes them up.
- Total memory is Chromium's: nine processes, **422 MB** PSS (850 MB summed RSS)
  for one static page without GPU acceleration. Three tabs of a heavy app will
  cost what they cost in any Chromium browser.
- Scrolling and typing are page-raster bound. If those feel slow, the wins are
  `--disable-accelerated-2d-canvas`, reducing zoom, and turning images off
  (`:images`).
