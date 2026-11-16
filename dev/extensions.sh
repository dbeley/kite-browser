#!/usr/bin/env bash
# Fetch the two addons that are worth having in kite, as unpacked directories.
#
#   ./dev/extensions.sh            # into ~/.local/share/kite/ext
#
# Then add the printed paths to `general.extensions` in your kite config.
# uBlock Origin classic (MV2) is dead in current Chromium; uBOLite is the MV3
# one. KeePassXC-Browser needs its id in KeePassXC's native-messaging manifest,
# which `:extensions` shows you (see the README).
set -euo pipefail

DEST="${1:-$HOME/.local/share/kite/ext}"
mkdir -p "$DEST"

fetch() { # name, repo, asset-pattern
  local name="$1" repo="$2" pattern="$3"
  local url
  url=$(curl -sfL "https://api.github.com/repos/$repo/releases/latest" \
    | python3 -c '
import json, sys
pattern = sys.argv[1]
d = json.load(sys.stdin)
for a in d.get("assets", []):
    if a["name"].endswith(pattern) and "chromium" in a["name"]:
        print(a["browser_download_url"])
        break
' "$pattern")
  if [ -z "$url" ]; then
    echo "$name: no chromium asset in the latest release" >&2
    return 1
  fi
  echo "$name: $url"
  rm -rf "$DEST/$name"
  mkdir -p "$DEST/$name"
  curl -sfL "$url" -o /tmp/kite-ext.zip
  python3 - "$DEST/$name" <<'PY'
import sys, zipfile
dest = sys.argv[1]
with zipfile.ZipFile("/tmp/kite-ext.zip") as z:
    z.extractall(dest)
    names = [n for n in z.namelist() if n.endswith("manifest.json")]
# Some zips wrap everything in one directory: lift it if so.
if names and "/" in names[0]:
    import os, shutil
    root = os.path.join(dest, names[0].split("/")[0])
    for entry in os.listdir(root):
        shutil.move(os.path.join(root, entry), dest)
    shutil.rmtree(root, ignore_errors=True)
PY
  rm -f /tmp/kite-ext.zip
  echo "$name: $(python3 -c 'import json,sys;m=json.load(open(sys.argv[1]));print(m.get("name"),m.get("version"))' "$DEST/$name/manifest.json")"
}

fetch ublock-origin-lite uBlockOrigin/uBOL-home ".chromium.zip"
fetch keepassxc-browser keepassxreboot/keepassxc-browser "_chromium.zip"

cat <<EOF

Add these to your kite config (~/.config/kite/config.toml):

[general]
extensions = [
  "$DEST/ublock-origin-lite",
  "$DEST/keepassxc-browser",
]
EOF
