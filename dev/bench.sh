#!/usr/bin/env bash
# Cold-start + memory comparison under a real X server. Runs three cold starts of
# each browser and reports the median, because this machine has 2 cores and the
# first run is always the worst.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DISP="${KITE_DISP:-:88}"
OUT="${1:-/tmp/kite-bench.jsonl}"
SERVE="${KITE_HOME:-/tmp/kite-bench}/serve"
mkdir -p "$SERVE" "$(dirname "$OUT")"
: > "$OUT"

cat > "$SERVE/index.html" <<'HTML'
<!doctype html><meta charset="utf-8"><title>bench page</title>
<style>body{font:16px/1.6 system-ui;margin:0}header{background:#111827;color:#fff;padding:40px}main{padding:30px}</style>
<header><h1>benchmark page</h1></header>
<main><p>Static text, a few links:</p>
<ul><li><a href="/a">one</a></li><li><a href="/b">two</a></li><li><a href="/c">three</a></li></ul>
</main>
HTML

Xvfb "$DISP" -screen 0 1280x900x24 -nolisten tcp >/dev/null 2>&1 &
XP=$!
(cd "$SERVE" && exec python3 -m http.server 8800 --bind 127.0.0.1 >/dev/null 2>&1) &
SP=$!
sleep 1.2
export DISPLAY="$DISP"
# NixOS' chromium wrapper switches to Wayland whenever WAYLAND_DISPLAY is set,
# which would put the window on the real desktop instead of Xvfb.
unset WAYLAND_DISPLAY WAYLAND_SOCKET
export KITE_NO_SANDBOX=1
# Only ever matches the throwaway bench profile, never the invoking shell.
export BENCH_KILLPAT="/tmp/kite-bench/st/engine-profile"

run3() {
  local label="$1"; shift
  local cls="$1"; shift
  for i in 1 2 3; do
    rm -rf "/tmp/kite-bench/prof-$label"
    BENCH_CLASS="$cls" BENCH_SETTLE="${BENCH_SETTLE:-12}" XDG_CONFIG_HOME="/tmp/kite-bench/cfg-$label" \
      python3 "$HERE/dev/bench.py" "$label" "$@" >> "$OUT"
    sleep 1
  done
}

echo "== kite (x3)"
run3 kite Chromium-browser "$HERE/target/release/kite" --state "/tmp/kite-bench/st" "http://127.0.0.1:8800/index.html"

if command -v qutebrowser >/dev/null 2>&1; then
  echo "== qutebrowser (x3)"
  run3 qutebrowser qutebrowser env QUTE_CONFIG_DIR=/tmp/kite-bench/qute-cfg qutebrowser --temp-basedir --backend webengine "http://127.0.0.1:8800/index.html"
else
  echo "== qutebrowser not installed; skipping comparison"
fi

pkill -f "Xvfb $DISP" 2>/dev/null
kill $SP 2>/dev/null
echo "== results ($OUT)"
cat "$OUT"
