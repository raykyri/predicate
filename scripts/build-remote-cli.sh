#!/usr/bin/env bash
set -euo pipefail

# Cross-compiles standalone linux-musl qmux-cli binaries for remote hosts.
# Requires: rustup, zig, cargo-zigbuild (`cargo install cargo-zigbuild`).

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null && pwd)"
repo_root="$(cd "$script_dir/.." >/dev/null && pwd)"
out_dir="$repo_root/src-tauri/remote-cli"
targets=(aarch64-unknown-linux-musl x86_64-unknown-linux-musl)
source_roots=(
  "$repo_root/src-tauri/crates/qmux-cli"
  "$repo_root/src-tauri/crates/qmux-proto"
  "$repo_root/src-tauri/Cargo.lock"
)

remote_cli_stale() {
  local target bin
  for target in "${targets[@]}"; do
    bin="$out_dir/$target/qmux-cli"
    [[ -f "$bin" ]] || return 0
    if [[ -n "$(find "${source_roots[@]}" \( -type f -o -type l \) -newer "$bin" -print -quit)" ]]; then
      return 0
    fi
  done
  return 1
}

if [[ "${QMUX_REBUILD_REMOTE_CLI:-}" == "1" ]]; then
  echo "QMUX_REBUILD_REMOTE_CLI=1; rebuilding linux-musl qmux-cli"
elif remote_cli_stale; then
  echo "linux-musl qmux-cli is missing or older than qmux-cli sources; rebuilding"
else
  echo "remote-cli artifacts are up to date with qmux-cli sources"
  exit 0
fi

if ! command -v zig >/dev/null; then
  echo "zig is required to build linux-musl qmux-cli (brew install zig)." >&2
  exit 1
fi
if ! command -v cargo-zigbuild >/dev/null && ! cargo zigbuild -V >/dev/null 2>&1; then
  echo "cargo-zigbuild is required (cargo install cargo-zigbuild)." >&2
  exit 1
fi

if command -v rustup >/dev/null; then
  for target in "${targets[@]}"; do
    rustup target add "$target"
  done
fi

for target in "${targets[@]}"; do
  cargo zigbuild --release -p qmux-cli \
    --target "$target" \
    --manifest-path "$repo_root/src-tauri/Cargo.toml"
  src="$repo_root/src-tauri/target/$target/release/qmux-cli"
  if [[ ! -f "$src" ]]; then
    echo "expected $src after cargo zigbuild" >&2
    exit 1
  fi
  dest_dir="$out_dir/$target"
  mkdir -p "$dest_dir"
  cp "$src" "$dest_dir/qmux-cli"
  chmod 755 "$dest_dir/qmux-cli"
  echo "wrote $dest_dir/qmux-cli"
done
