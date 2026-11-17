#!/usr/bin/env bash
# Headless GUI harness: Xvfb + i3 window manager, then kite on top of Chromium.
# Everything here is real: a real X server, a real WM, a real Chromium process.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
DISP="${KITE_DISP:-:99}"
SCREEN="${KITE_SCREEN:-1280x900x24}"
OUT="${KITE_OUT:-/tmp/kite-shots}/${KITE_SCENARIO:-basic}"
STATE="${KITE_HOME:-/tmp/kite-state}"
SRV="$STATE/serve"

pkill -f "Xvfb $DISP" 2>/dev/null || true
pkill -f "kite.*--state" 2>/dev/null || true
sleep 0.3
rm -rf "$OUT" "$SRV" "$STATE/engine-profile" "$STATE/session.json" "$STATE/closed.json"
mkdir -p "$OUT" "$SRV"

# ---------------------------------------------------------------- test pages
cat > "$SRV/index.html" <<'HTML'
<!doctype html><html><head><meta charset="utf-8"><title>kite test page</title>
<style>
  :root { color-scheme: light; }
  body { font: 16px/1.55 system-ui, sans-serif; margin: 0; background: #fbfbfd; color: #1b1f24; }
  .hero { background: linear-gradient(135deg,#1f2937,#111827); color: #f9fafb; padding: 42px 56px; }
  .hero h1 { margin: 0 0 6px; font-size: 34px; letter-spacing: -.5px; }
  .hero p { margin: 0; color: #9ca3af; }
  main { padding: 34px 56px 90px; max-width: 940px; }
  h2 { font-size: 19px; margin: 30px 0 10px; }
  a { color: #1d4ed8; text-decoration: none; border-bottom: 1px solid #c7d2fe; }
  li { margin: 7px 0; }
  .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
  .card { border: 1px solid #e5e7eb; border-radius: 9px; padding: 14px 16px; background: #fff; }
  button { font: inherit; padding: 7px 14px; border-radius: 7px; border: 1px solid #cbd5e1; background: #fff; cursor: pointer; }
  input { font: inherit; padding: 8px 11px; border-radius: 7px; border: 1px solid #cbd5e1; width: 300px; }
  .spacer { height: 900px; }
</style></head><body>
<div class="hero"><h1>kite test page</h1><p>links, buttons and a form — enough to exercise hints, find, scroll and insert mode</p></div>
<main>
  <h2>Links</h2>
  <ul>
    <li><a href="/one.html" id="l1">first link — local page one</a></li>
    <li><a href="/two.html" id="l2">second link — local page two</a></li>
    <li><a href="https://example.com/" id="l3">external — example.com</a></li>
    <li><a href="#anchor">in-page anchor</a></li>
  </ul>
  <h2>Controls</h2>
  <div class="grid">
    <div class="card"><p>A button, for hint-mode clicks:</p><button id="b1" onclick="document.getElementById('hits').textContent='button clicked'">Click target</button></div>
    <div class="card"><p>Type here (insert mode):</p><input id="q" placeholder="search terms…"></div>
  </div>
  <p id="hits" style="color:#0f766e;font-weight:600">—</p>
  <h2>Scrolling</h2>
  <p id="scroll-target">Scroll target: use j, C-d, G, gg.</p>
  <div class="spacer"></div>
  <p style="color:#6b7280">end of page</p>
</main></body></html>
HTML
cat > "$SRV/one.html" <<'HTML'
<!doctype html><meta charset="utf-8"><title>page one</title>
<body style="font:17px/1.6 system-ui;padding:48px"><h1>Page one</h1><p>Navigated. <a href="/index.html">back to index</a></p></body>
HTML
cat > "$SRV/two.html" <<'HTML'
<!doctype html><meta charset="utf-8"><title>page two</title>
<body style="font:17px/1.6 system-ui;padding:48px"><h1>Page two</h1><p>Second target. <a href="/index.html">back to index</a></p></body>
HTML
# A framed page: child frames get `kite` worlds of their own, which once stole
# the tab's context id and left the palette silently empty.
cat > "$SRV/framed.html" <<'HTML'
<!doctype html><meta charset="utf-8"><title>page with a frame</title>
<body style="font:17px/1.6 system-ui;padding:48px"><h1>Page with a frame</h1>
<iframe src="/one.html" width="420" height="160"></iframe></body>
HTML

# ------------------------------------------------------------------- servers
mkdir -p "$STATE/i3"
cat > "$STATE/i3/config" <<'I3'
# minimal, prompt-free i3 for the harness
font pango:monospace 8
focus_follows_mouse no
new_window none
new_float none
workspace_layout default
bar { mode invisible }
I3

( cd "$SRV" && exec python3 -m http.server 8808 --bind 127.0.0.1 >/dev/null 2>&1 ) &
SRV_PID=$!
Xvfb "$DISP" -screen 0 "$SCREEN" -nolisten tcp >/dev/null 2>&1 &
XVFB_PID=$!
sleep 0.8
DISPLAY="$DISP" i3 -c "$STATE/i3/config" >/dev/null 2>&1 &
I3_PID=$!
sleep 1.2
curl -sf --max-time 3 http://127.0.0.1:8808/index.html >/dev/null && echo "server: ok" || echo "server: FAILED"
trap 'kill $SRV_PID $XVFB_PID $I3_PID 2>/dev/null || true; pkill -f "user-data-dir=$STATE" 2>/dev/null || true' EXIT

export DISPLAY="$DISP"
# NixOS' chromium wrapper switches to Wayland whenever WAYLAND_DISPLAY is set,
# which would put the window on the developer's real desktop instead of Xvfb.
unset WAYLAND_DISPLAY WAYLAND_SOCKET
export KITE_HOME="$STATE"
export XDG_CONFIG_HOME="$STATE/xdg-config"
export KITE_NO_SANDBOX=1
export KITE_DISABLE_GPU="${KITE_DISABLE_GPU:-}"
export KITE_ENGINE_LOG="${KITE_ENGINE_LOG:-}"

shot() {
  local w="${WID:-}"
  [ -z "$w" ] && w=$(xdotool search --class "Chromium-browser" 2>/dev/null | tail -1)
  sleep "${2:-1.0}"
  if [ -n "$w" ]; then
    import -window "$w" "$OUT/$1.png" 2>/dev/null
    import -window root "$OUT/$1.root.png" 2>/dev/null
    echo "shot: $OUT/$1.png wid=$w $(identify -format '%wx%h sd=%[fx:standard_deviation]' "$OUT/$1.png" 2>/dev/null) | root sd=$(identify -format '%[fx:standard_deviation]' "$OUT/$1.root.png" 2>/dev/null)"
  fi
}
keys() { xdotool key --clearmodifiers --delay 60 "$@"; }
type_in() { xdotool type --clearmodifiers --delay 28 "$1"; }

# KITE_URL="" opens the built-in start page (no URL at all).
URL="${KITE_URL-http://127.0.0.1:8808/index.html}"
"$HERE/target/release/kite" ${URL:+"$URL"} &
KITE_PID=$!

# Wait for the window to exist and paint (software raster is slow here).
for _ in $(seq 1 60); do
  if xdotool search --class "Chromium-browser" >/dev/null 2>&1; then break; fi
  sleep 0.5
done
sleep "${KITE_SETTLE:-2}"

# Focus the browser window (i3 tiles it; the page surface is Chromium-browser).
WID=$(xdotool search --class "Chromium-browser" 2>/dev/null | tail -1 || true)
if [ -n "${WID:-}" ]; then xdotool windowactivate --sync "$WID" 2>/dev/null || true; fi
sleep 0.5

echo "--- scenario: ${KITE_SCENARIO:-basic}"
case "${KITE_SCENARIO:-basic}" in
  basic)
    shot 01-loaded
    keys k; shot 02-bar
    keys j j j j; shot 03-scrolled
    keys f; shot 04-hints
    ;;
  palette)
    keys colon; type_in "ope"; shot 01-palette
    ;;
  palette-framed)
    # The omnibox must fill in even when the page has a child frame.
    keys o; type_in "127.0.0.1:8808/framed.html"; keys Return; sleep 3
    keys o; type_in "one"; shot 01-palette-framed
    ;;
  help)
    keys question; shot 01-help
    ;;
  tabs)
    keys t; sleep 1.2
    keys t; sleep 1.2
    keys k; shot 01-tabs
    ;;
  insert)
    keys g i; shot 01-insert
    ;;
  nav)
    keys o; type_in "127.0.0.1:8808/one.html"; keys Return; sleep 3; shot 01-nav
    ;;
  find)
    keys slash; type_in "anchor"; keys Return; sleep 1; shot 01-find
    ;;
  home)
    shot 01-home
    keys o; type_in "127.0.0.1:8808/one.html"; keys Return; sleep 3; shot 02-home-nav
    ;;
  *)
    shot 01-start
    ;;
esac

[ "${KITE_KEEP:-0}" = "1" ] && { echo "keeping session: kite pid $KITE_PID"; wait $KITE_PID; }
kill "$KITE_PID" 2>/dev/null || true
wait "$KITE_PID" 2>/dev/null || true
