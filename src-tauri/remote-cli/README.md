# Bundled remote `qmux-cli`

Linux-musl binaries that qmux can push to a remote host live here after
`scripts/build-remote-cli.sh`. Shipping builds (`scripts/build.sh`) run that
script so the app bundle always contains:

- `aarch64-unknown-linux-musl/qmux-cli`
- `x86_64-unknown-linux-musl/qmux-cli`

`tauri dev` does not require these files. If the remote already has a matching
`~/.qmux/bin/qmux-cli`, Test connection skips the bundle. A Linux host that
needs an install will error with `run scripts/build-remote-cli.sh`.

`scripts/build.sh` zigbuilds when an artifact is missing or older than
`crates/qmux-cli`, `crates/qmux-proto`, or `Cargo.lock`. `scripts/release.sh`
sets `QMUX_REBUILD_REMOTE_CLI=1` to rebuild regardless. Requires Zig
(`brew install zig`) and `cargo install cargo-zigbuild`.
