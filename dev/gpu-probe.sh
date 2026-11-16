#!/usr/bin/env bash
# Probe which Chromium flag set actually paints in a headless X server.
set -uo pipefail
OUT=/tmp/gpuprobe
rm -rf "$OUT"; mkdir -p "$OUT"
PAGE=/tmp/gpuprobe.html
cat > "$PAGE" <<'H'
<!doctype html><meta charset=utf-8><title>probe</title>
<body style="margin:0;font:24px system-ui">
<div style="background:linear-gradient(135deg,#1f2937,#111827);color:#fff;padding:40px">hello probe</div>
<p style="padding:20px">text block <a href="#">a link</a></p>
</body>
H

run_variant() {
  local name="$1"; shift
  local disp=":9$((RANDOM % 9))"
  Xvfb "$disp" -screen 0 1024x700x24 -nolisten tcp >/dev/null 2>&1 &
  local xp=$!
  sleep 0.7
  DISPLAY="$disp" i3 -c /tmp/gpuprobe-i3 >/dev/null 2>&1 &
  local ip=$!
  sleep 0.7
  DISPLAY="$disp" chromium --app="file://$PAGE" --user-data-dir="$OUT/prof-$name" \
    --no-sandbox --test-type --no-first-run --window-size=1024,700 "$@" >"$OUT/$name.log" 2>&1 &
  local cp=$!
  sleep 5
  DISPLAY="$disp" import -window root "$OUT/$name.png" 2>/dev/null
  local sd
  sd=$(identify -format "%[fx:standard_deviation]" "$OUT/$name.png" 2>/dev/null || echo 0)
  echo "$name: sd=$sd  ($(stat -c%s "$OUT/$name.png" 2>/dev/null) bytes)"
  kill $cp $ip $xp 2>/dev/null
  wait $cp 2>/dev/null
  sleep 0.3
}

cat > /tmp/gpuprobe-i3 <<'I'
font pango:monospace 8
bar { mode invisible }
I

run_variant default
run_variant nogpu --disable-gpu
run_variant swiftshader --enable-unsafe-swiftshader --use-gl=angle --use-angle=swiftshader
run_variant gpucompositing-off --disable-gpu-compositing
run_variant x11 --ozone-platform=x11 --use-gl=egl
