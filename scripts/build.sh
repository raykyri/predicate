#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null && pwd)"

# Use the repository's local build configuration when the caller did not
# provide a GitHub OAuth client ID explicitly. Automatically export sourced
# values so they are available to the Tauri and Cargo subprocesses.
if [[ -z "${QMUX_GITHUB_CLIENT_ID:-}" && -f "$script_dir/../.env" ]]; then
  set -a
  source "$script_dir/../.env"
  set +a
fi

# `tauri` lives in node_modules/.bin; put it on PATH so this script also works
# when invoked directly (e.g. from release.sh) rather than through `npm run`.
export PATH="$script_dir/../node_modules/.bin:$PATH"

# Shipped bundles must include the Foundation Models tab-title bridge; without
# this the bridge is optional and a missing Swift toolchain only warns.
export QMUX_REQUIRE_FOUNDATION_MODELS=1

# createUpdaterArtifacts makes the bundler sign the updater .tar.gz, which fails
# without the private half of the updater keypair. Pick up the local key when the
# caller didn't provide one (CI should set TAURI_SIGNING_PRIVATE_KEY instead; the
# variable accepts either the key contents or a path to the key file).
default_updater_key="$HOME/.tauri/qmux-updater.key"
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" && -f "$default_updater_key" ]]; then
  export TAURI_SIGNING_PRIVATE_KEY="$default_updater_key"
fi

# Tauri does not discover that a configured signing identity is inaccessible
# until after the application has been built and bundled. In restricted
# environments such as the Codex sandbox, the login keychain can be present but
# hidden from `security` and `codesign`. Fail before the two universal-arch Rust
# builds instead of spending several minutes on an artifact that cannot be
# signed.
if [[ "$(uname -s)" == "Darwin" && -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  valid_signing_identities="$(security find-identity -v -p codesigning 2>/dev/null || true)"
  if ! grep -Fq "\"$APPLE_SIGNING_IDENTITY\"" <<<"$valid_signing_identities"; then
    echo "Configured Apple signing identity is not available to this process:" >&2
    echo "  $APPLE_SIGNING_IDENTITY" >&2
    echo "Run 'security find-identity -v -p codesigning' in the same environment" >&2
    echo "and grant that environment access to the login keychain." >&2
    exit 1
  fi
fi

# Release DMGs must run on both Apple Silicon and Intel Macs, so default to a
# universal binary. Override with e.g. QMUX_BUILD_TARGET=aarch64-apple-darwin
# for a faster single-arch build.
build_target="${QMUX_BUILD_TARGET:-universal-apple-darwin}"

if [[ "$build_target" == "universal-apple-darwin" ]] && command -v rustup >/dev/null; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
fi

"$script_dir/cleanup-tauri-dmg.sh"

# Tauri's DMG bundler runs an AppleScript step that drives Finder to lay out the
# disk-image window. Run that normal path by default, matching Trajectories, so
# successful local and release builds always carry the polished window and icon
# placement. Headless callers can explicitly set CI=true; Tauri then passes
# --skip-jenkins and produces the plain, non-interactive fallback instead.
tauri build --target "$build_target"
