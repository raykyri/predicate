#!/usr/bin/env bash
set -euo pipefail

# Generates the latest.json update manifest the updater plugin polls at
# https://github.com/raykyri/qmux/releases/latest/download/latest.json.
# Run after scripts/build.sh, then upload latest.json AND the .app.tar.gz +
# .sig to the GitHub release alongside the DMG.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." >/dev/null && pwd)"
target="${QMUX_BUILD_TARGET:-universal-apple-darwin}"
bundle_dir="$repo_root/src-tauri/target/$target/release/bundle/macos"

version="$(sed -n 's/.*"version": "\([^"]*\)".*/\1/p' "$repo_root/src-tauri/tauri.conf.json" | head -1)"
archive="$bundle_dir/qmux.app.tar.gz"
signature_file="$archive.sig"

for file in "$archive" "$signature_file"; do
  if [[ ! -f "$file" ]]; then
    echo "Missing $file — run scripts/build.sh first." >&2
    exit 1
  fi
done

signature="$(cat "$signature_file")"
url="https://github.com/raykyri/qmux/releases/download/v$version/qmux.app.tar.gz"
pub_date="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Build the manifest with Python so signature bytes never re-enter the shell
# parser (unquoted heredocs expand $(...), backticks, and similar).
if ! command -v python3 >/dev/null; then
  echo "python3 is required to write latest.json safely." >&2
  exit 1
fi
SIGNATURE="$signature" VERSION="$version" PUB_DATE="$pub_date" URL="$url" \
  python3 - <<'PY' >"$bundle_dir/latest.json"
import json, os
print(json.dumps({
    "version": os.environ["VERSION"],
    "pub_date": os.environ["PUB_DATE"],
    "platforms": {
        "darwin-aarch64": {
            "signature": os.environ["SIGNATURE"],
            "url": os.environ["URL"],
        },
        "darwin-x86_64": {
            "signature": os.environ["SIGNATURE"],
            "url": os.environ["URL"],
        },
    },
}, indent=2))
print()
PY

echo "Wrote $bundle_dir/latest.json"
echo "Upload these to the v$version GitHub release:"
echo "  $archive (as qmux.app.tar.gz)"
echo "  $bundle_dir/latest.json"
