#!/usr/bin/env bash
# qutebrowser side of the benchmark. Kept in a file (not `bash -c`) so that
# BENCH_KILLPAT never appears in this shell's argv — otherwise pkill -f would
# kill the benchmark itself.
set -uo pipefail
export DISPLAY=:86
export QT_QPA_PLATFORM=xcb
export QUTE_CONFIG_DIR=/tmp/qbench/cfg
export BENCH_KILLPAT="/tmp/qbench/cfg"
mkdir -p /tmp/qbench /tmp/qbench/serve

cat > /tmp/qbench/serve/index.html <<'HTML'
<!doctype html><meta charset="utf-8"><title>bench page</title>
<style>body{font:16px/1.6 system-ui;margin:0}header{background:#111827;color:#fff;padding:40px}main{padding:30px}</style>
<header><h1>benchmark page</h1></header>
<main><p>Static text, a few links:</p>
<ul><li><a href="/a">one</a></li><li><a href="/b">two</a></li><li><a href="/c">three</a></li></ul>
</main>
HTML

Xvfb :86 -screen 0 1280x900x24 -nolisten tcp >/dev/null 2>&1 &
XP=$!
( cd /tmp/qbench/serve && exec python3 -m http.server 8802 --bind 127.0.0.1 >/dev/null 2>&1 ) &
SP=$!
sleep 1.5

BENCH_PY=/home/david/workspace/projects/kite-browser/dev/bench.py
for i in 1 2 3; do
  rm -rf /tmp/qbench/cfg
  BENCH_CLASS=qutebrowser BENCH_SETTLE=15 BENCH_TIMEOUT=150 \
    python3 "$BENCH_PY" qutebrowser qutebrowser --temp-basedir --backend webengine \
    --target window "http://127.0.0.1:8802/index.html"
  sleep 1
done

kill $XP $SP 2>/dev/null
