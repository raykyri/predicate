use crate::adapters::{AdapterMetadata, adapter_registry};
use crate::title_generation;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QmuxConfig {
    pub workspace_root: PathBuf,
    pub socket_path: PathBuf,
    #[serde(default)]
    pub adapters: AdapterConfigs,
    #[serde(
        default,
        rename = "claudeBinary",
        skip_serializing_if = "Option::is_none"
    )]
    pub legacy_claude_binary: Option<String>,
    /// Directory of the qmux-managed Claude plugin whose `skills/` are injected
    /// into launched Claude agents via `--plugin-dir`. Resolved at load time from
    /// `QMUX_CLAUDE_PLUGIN_DIR` or `<cwd>/qmux-claude-plugin`; never read from or written
    /// to the config JSON (it is derived, not configured).
    #[serde(skip)]
    pub claude_plugin_dir: PathBuf,
    /// Directory of the qmux-managed opencode plugin whose JS files are injected
    /// into launched opencode agents via `OPENCODE_CONFIG_DIR`. Resolved at load
    /// time from `QMUX_OPENCODE_PLUGIN_DIR` or `<cwd>/qmux-opencode-plugin`; never
    /// read from or written to the config JSON (it is derived, not configured).
    #[serde(skip)]
    pub opencode_plugin_dir: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterConfigs {
    #[serde(default)]
    pub claude: ClaudeAdapterConfig,
    #[serde(default)]
    pub codex: CodexAdapterConfig,
    #[serde(default)]
    pub opencode: OpencodeAdapterConfig,
    #[serde(default)]
    pub grok: GrokAdapterConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAdapterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAdapterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeAdapterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokAdapterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub workspace_root: String,
    pub socket_path: String,
    pub adapters: Vec<AdapterMetadata>,
    // The user's home directory, so the UI can render home-relative paths as ~/…
    // instead of bare relative segments. Empty if HOME is unset.
    pub home_dir: String,
    pub tab_title_generation: TabTitleGenerationRuntimeConfig,
    // Port of the loopback file server, so the frontend can recognize token-bearing
    // file-server URLs and force them to load sandboxed (never as a same-origin
    // document that could read the token back). Filled in by `get_runtime_config`
    // from live state after the server binds; `None` here since config alone can't
    // know the ephemeral port.
    pub file_server_port: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabTitleGenerationRuntimeConfig {
    pub apple_foundation_models_available: bool,
}

impl QmuxConfig {
    pub fn load() -> Result<Self, String> {
        let cwd = env::current_dir().map_err(|err| format!("failed to read cwd: {err}"))?;

        // Which config applies:
        // - `QMUX_CONFIG=<file>` is explicit intent, honored in every build; a
        //   missing or malformed file is an error rather than a silent fallback.
        // - Otherwise dev builds discover `<cwd>/qmux.config.json`, so a checkout
        //   keeps its state in `<repo>/.qmux` when run from the repo.
        // - Release builds never read a config from the cwd: the persisted session
        //   must live in the same place no matter how the app is launched (Finder
        //   gives cwd `/`, a terminal gives the project directory), otherwise each
        //   launch style reads and writes its own divergent session history.
        let explicit = env::var_os("QMUX_CONFIG").filter(|value| !value.is_empty());
        let (mut config, config_dir) = if let Some(explicit) = explicit {
            let path = absolutize(&cwd, Path::new(&explicit));
            let dir = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| cwd.clone());
            (Self::read_config_file(&path)?, dir)
        } else if let Some(discovered) = Self::discover_dev_config(&cwd)? {
            (discovered, cwd.clone())
        } else {
            (Self::default_config()?, cwd.clone())
        };

        // `~/…` workspace/socket paths expand against the user's home and absolute
        // paths are honored verbatim. Relative paths are resolved against the
        // directory of the config file that declared them, and only when that
        // directory is inside the user's home; otherwise they fall back to the
        // home-based data dir. This keeps a config sitting at the filesystem root
        // or in a system directory from materializing a `.qmux` outside userspace.
        let home = env::var_os("HOME").map(PathBuf::from);
        let default_workspace_root = qmux_data_root().map(|root| root.join("workspaces"));
        let default_socket_path = qmux_runtime_root().map(|root| root.join("qmux.sock"));
        config.workspace_root = resolve_root(
            &config_dir,
            home.as_deref(),
            &config.workspace_root,
            default_workspace_root.as_deref(),
        );
        config.socket_path = resolve_root(
            &config_dir,
            home.as_deref(),
            &config.socket_path,
            default_socket_path.as_deref(),
        );
        config.claude_plugin_dir = resolve_claude_plugin_dir(&cwd);
        config.opencode_plugin_dir = resolve_opencode_plugin_dir(&cwd);

        fs::create_dir_all(&config.workspace_root).map_err(|err| {
            format!(
                "failed to create workspace root {}: {err}",
                config.workspace_root.display()
            )
        })?;

        // Keep qmux's private state tree owner-only, matching the 0700/0600 treatment
        // the control socket and shell-integration files already get. `.qmux` holds the
        // persisted state (composer drafts, queued-turn prompts), preferences, hook
        // settings, and per-pane terminal scrollback logs — which can capture any secret
        // echoed to a terminal and the pane's own QMUX_TOKEN. Runs on every startup so a
        // tree created by an older, unhardened build is tightened on the next launch.
        // Best-effort: the individual writers below also create their files 0600.
        {
            use std::os::unix::fs::PermissionsExt;
            let state_dir = config.workspace_root.join(".qmux");
            if fs::create_dir_all(&state_dir).is_ok() {
                let _ = fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700));
            }
        }

        if let Some(parent) = config.socket_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "failed to create socket directory {}: {err}",
                    parent.display()
                )
            })?;
            // Best-effort: keep the control socket's directory owner-only so the socket
            // itself is not reachable by other local accounts.
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }

        Ok(config)
    }

    pub fn runtime(&self) -> RuntimeConfig {
        RuntimeConfig {
            workspace_root: self.workspace_root.display().to_string(),
            socket_path: self.socket_path.display().to_string(),
            adapters: adapter_registry(self).metadata(),
            home_dir: env::var("HOME").unwrap_or_default(),
            tab_title_generation: TabTitleGenerationRuntimeConfig {
                apple_foundation_models_available: title_generation::foundation_models_available(),
            },
            file_server_port: None,
        }
    }

    pub fn claude_binary(&self) -> String {
        expand_binary(
            self.adapters
                .claude
                .binary
                .clone()
                .or_else(|| self.legacy_claude_binary.clone())
                .unwrap_or_else(|| "claude".to_string()),
        )
    }

    pub fn codex_binary(&self) -> String {
        expand_binary(
            self.adapters
                .codex
                .binary
                .clone()
                .unwrap_or_else(|| "codex".to_string()),
        )
    }

    pub fn opencode_binary(&self) -> String {
        expand_binary(
            self.adapters
                .opencode
                .binary
                .clone()
                .unwrap_or_else(|| "opencode".to_string()),
        )
    }

    pub fn grok_binary(&self) -> String {
        expand_binary(
            self.adapters
                .grok
                .binary
                .clone()
                .unwrap_or_else(|| "grok".to_string()),
        )
    }

    fn read_config_file(path: &Path) -> Result<Self, String> {
        let raw = fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        serde_json::from_str::<QmuxConfig>(&raw)
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))
    }

    /// Debug builds pick up a `qmux.config.json` from the process cwd so a dev
    /// checkout keeps its own state; release builds never do (see `load`).
    fn discover_dev_config(cwd: &Path) -> Result<Option<Self>, String> {
        #[cfg(debug_assertions)]
        {
            let path = cwd.join("qmux.config.json");
            if path.exists() {
                return Self::read_config_file(&path).map(Some);
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = cwd;
        Ok(None)
    }

    fn default_config() -> Result<Self, String> {
        let data_root =
            qmux_data_root().ok_or_else(|| "could not determine data directory".to_string())?;
        let runtime_root = qmux_runtime_root()
            .ok_or_else(|| "could not determine runtime directory".to_string())?;
        Ok(Self {
            workspace_root: data_root.join("workspaces"),
            socket_path: runtime_root.join("qmux.sock"),
            adapters: AdapterConfigs {
                claude: ClaudeAdapterConfig {
                    binary: Some("claude".to_string()),
                },
                codex: CodexAdapterConfig {
                    binary: Some("codex".to_string()),
                },
                opencode: OpencodeAdapterConfig {
                    binary: Some("opencode".to_string()),
                },
                grok: GrokAdapterConfig {
                    binary: Some("grok".to_string()),
                },
            },
            legacy_claude_binary: None,
            // Overwritten by load() once the cwd is known; this default is only a
            // placeholder for the no-config-file path.
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
        })
    }
}

/// Resolves the qmux-managed Claude plugin directory. Honors an explicit
/// `QMUX_CLAUDE_PLUGIN_DIR` override (absolutized against the cwd when relative);
/// otherwise picks the first existing candidate so skills load regardless of how
/// qmux is launched — not only when the process cwd happens to be the repo root.
fn resolve_claude_plugin_dir(cwd: &Path) -> PathBuf {
    let override_os = env::var_os("QMUX_CLAUDE_PLUGIN_DIR").filter(|value| !value.is_empty());
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    pick_claude_plugin_dir(
        cwd,
        override_os.as_deref().map(Path::new),
        exe_dir.as_deref(),
    )
}

/// Pure resolver (testable): the explicit override always wins; otherwise the first
/// existing candidate is used, falling back to `<cwd>/qmux-claude-plugin` when none exist.
/// Candidates cover the ways qmux runs:
/// - `<cwd>/qmux-claude-plugin` — dev (`tauri dev`) or a binary run from the repo root.
/// - `<exe_dir>/qmux-claude-plugin` — plugin copied next to the binary.
/// - `<exe_dir>/../Resources/qmux-claude-plugin` — the macOS `.app` bundle (Finder launch).
/// - `<exe_dir>/../../../qmux-claude-plugin` — `src-tauri/target/<profile>/qmux` -> repo root.
fn pick_claude_plugin_dir(
    cwd: &Path,
    override_dir: Option<&Path>,
    exe_dir: Option<&Path>,
) -> PathBuf {
    pick_plugin_dir(cwd, override_dir, exe_dir, "qmux-claude-plugin")
}

/// Resolves the qmux-managed opencode plugin directory. Honors an explicit
/// `QMUX_OPENCODE_PLUGIN_DIR` override (absolutized against the cwd when relative);
/// otherwise picks the first existing candidate so the plugin loads regardless of
/// how qmux is launched. Mirrors `resolve_claude_plugin_dir`.
fn resolve_opencode_plugin_dir(cwd: &Path) -> PathBuf {
    let override_os = env::var_os("QMUX_OPENCODE_PLUGIN_DIR").filter(|value| !value.is_empty());
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    pick_plugin_dir(
        cwd,
        override_os.as_deref().map(Path::new),
        exe_dir.as_deref(),
        "qmux-opencode-plugin",
    )
}

/// Shared plugin-directory resolver used by both the Claude and opencode plugin
/// lookups. The explicit override always wins; otherwise the first existing
/// candidate is used, falling back to `<cwd>/<default_name>` when none exist.
fn pick_plugin_dir(
    cwd: &Path,
    override_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    default_name: &str,
) -> PathBuf {
    if let Some(override_dir) = override_dir {
        return absolutize(cwd, override_dir);
    }
    let mut candidates = vec![cwd.join(default_name)];
    if let Some(exe_dir) = exe_dir {
        candidates.push(exe_dir.join(default_name));
        candidates.push(exe_dir.join("../Resources").join(default_name));
        candidates.push(exe_dir.join("../../../").join(default_name));
    }
    candidates
        .into_iter()
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| cwd.join(default_name))
}

/// Base directory for qmux's persistent data (workspaces and persisted state)
/// when it isn't being resolved relative to a project cwd. Platform-conventional:
/// `~/Library/Application Support/qmux` on macOS and `$XDG_DATA_HOME/qmux`
/// (`~/.local/share/qmux`) on Linux. `None` only when the home directory can't be
/// determined.
fn qmux_data_root() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("qmux"))
}

/// Directory for qmux's control socket. On Linux this is the per-user runtime dir
/// (`$XDG_RUNTIME_DIR/qmux`, a tmpfs owned 0700 by the user); where no runtime dir
/// exists (macOS, or `$XDG_RUNTIME_DIR` unset) it falls back to a `run/` subdir of
/// the persistent data root.
fn qmux_runtime_root() -> Option<PathBuf> {
    dirs::runtime_dir()
        .map(|dir| dir.join("qmux"))
        .or_else(|| qmux_data_root().map(|dir| dir.join("run")))
}

/// Whether `cwd` sits inside the user's home directory, the condition under which
/// a relative workspace/socket path is allowed to resolve against it.
fn cwd_is_within_home(cwd: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| !home.as_os_str().is_empty() && cwd.starts_with(home))
}

/// Resolves a configured workspace/socket root to an absolute path. `~`/`~/…`
/// paths expand against the user's home (JSON can't otherwise express a
/// home-relative path portably) and absolute paths are honored verbatim — both
/// are explicit intent. A relative path resolves against `cwd` only when `cwd`
/// is inside the user's home; otherwise it falls back to `default_root` (the
/// home-based data dir) so a relative `.qmux` is never written into a system
/// directory or at the filesystem root. With no home to fall back to, the
/// relative path is resolved against `cwd` as a last resort.
fn resolve_root(
    cwd: &Path,
    home: Option<&Path>,
    configured: &Path,
    default_root: Option<&Path>,
) -> PathBuf {
    if let Some(expanded) = expand_home(configured, home) {
        return expanded;
    }
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    if cwd_is_within_home(cwd, home) {
        return cwd.join(configured);
    }
    default_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.join(configured))
}

/// Expands a leading `~` or `~/…` against the user's home directory. `~user`
/// forms are not supported (`strip_prefix` matches whole components, so a
/// `~user/…` path does not strip). Returns `None` — falling through to relative
/// resolution — when the path doesn't start with `~` or no home is known.
fn expand_home(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let home = home.filter(|home| !home.as_os_str().is_empty())?;
    let stripped = path.strip_prefix("~").ok()?;
    Some(home.join(stripped))
}

/// Expands a leading `~`/`~/…` in a configured adapter binary against `$HOME`, so a
/// `"binary": "~/bin/claude"` behaves like the tilde expansion documented for the
/// workspace/socket roots (the spawn is exec, not a shell, so it can't expand `~`
/// itself). A bare command name (`claude`) or an absolute path is returned unchanged
/// — a command name for a normal PATH lookup, an absolute path verbatim.
fn expand_binary(binary: String) -> String {
    if !binary.starts_with('~') {
        return binary;
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    expand_home(Path::new(&binary), home.as_deref())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(binary)
}

fn absolutize(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_binary_overrides_legacy_claude_binary() {
        let config: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "claudeBinary": "legacy-claude",
              "adapters": {
                "claude": {
                  "binary": "adapter-claude"
                }
              }
            }"#,
        )
        .unwrap();

        assert_eq!(config.claude_binary(), "adapter-claude");
    }

    #[test]
    fn legacy_claude_binary_is_used_when_adapter_binary_is_absent() {
        let config: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "claudeBinary": "legacy-claude"
            }"#,
        )
        .unwrap();

        assert_eq!(config.claude_binary(), "legacy-claude");
    }

    #[test]
    fn codex_binary_defaults_and_can_be_configured() {
        let default_config: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock"
            }"#,
        )
        .unwrap();
        assert_eq!(default_config.codex_binary(), "codex");

        let configured: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "adapters": {
                "codex": {
                  "binary": "/opt/bin/codex"
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(configured.codex_binary(), "/opt/bin/codex");
    }

    #[test]
    fn tilde_binary_expands_against_home() {
        let configured: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "adapters": { "claude": { "binary": "~/bin/claude" } }
            }"#,
        )
        .unwrap();
        if let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) {
            let expected = Path::new(&home)
                .join("bin/claude")
                .to_string_lossy()
                .into_owned();
            assert_eq!(configured.claude_binary(), expected);
        }

        // Bare command names (PATH lookup) and absolute paths are never rewritten.
        let plain: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "adapters": { "codex": { "binary": "/opt/bin/codex" } }
            }"#,
        )
        .unwrap();
        assert_eq!(plain.codex_binary(), "/opt/bin/codex");
        assert_eq!(plain.claude_binary(), "claude");
    }

    #[test]
    fn opencode_binary_defaults_and_can_be_configured() {
        let default_config: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock"
            }"#,
        )
        .unwrap();
        assert_eq!(default_config.opencode_binary(), "opencode");

        let configured: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "adapters": {
                "opencode": {
                  "binary": "/opt/bin/opencode"
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(configured.opencode_binary(), "/opt/bin/opencode");
    }

    #[test]
    fn grok_binary_defaults_and_can_be_configured() {
        let default_config: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock"
            }"#,
        )
        .unwrap();
        assert_eq!(default_config.grok_binary(), "grok");

        let configured: QmuxConfig = serde_json::from_str(
            r#"{
              "workspaceRoot": ".qmux/workspaces",
              "socketPath": ".qmux/run/qmux.sock",
              "adapters": {
                "grok": {
                  "binary": "/opt/bin/grok"
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(configured.grok_binary(), "/opt/bin/grok");
    }

    #[test]
    fn default_paths_live_under_platform_data_dir() {
        let config = QmuxConfig::default_config().unwrap();
        let data_root = dirs::data_dir().unwrap().join("qmux");

        // Workspaces and persisted state live under the platform data dir
        // (~/Library/Application Support/qmux on macOS, $XDG_DATA_HOME/qmux on Linux).
        assert_eq!(config.workspace_root, data_root.join("workspaces"));

        // The socket lives in the per-user runtime dir on Linux, else the data dir's
        // run/ subdir — never the shared system temp dir.
        let expected_socket = dirs::runtime_dir()
            .map(|dir| dir.join("qmux"))
            .unwrap_or_else(|| data_root.join("run"))
            .join("qmux.sock");
        assert_eq!(config.socket_path, expected_socket);
        assert_ne!(config.socket_path.parent(), Some(env::temp_dir().as_path()));
    }

    #[test]
    fn tilde_root_expands_against_home_from_any_cwd() {
        let home = Path::new("/Users/tester");
        let default = Path::new("/Users/tester/qmux/workspaces");
        // Expansion is cwd-independent: a Finder launch (cwd `/`) and a repo
        // launch resolve to the same place.
        for cwd in [Path::new("/"), Path::new("/Users/tester/Code/project")] {
            assert_eq!(
                resolve_root(
                    cwd,
                    Some(home),
                    Path::new("~/.qmux/workspaces"),
                    Some(default)
                ),
                PathBuf::from("/Users/tester/.qmux/workspaces")
            );
        }
        // `~user` is not expansion syntax; it stays a relative path and falls
        // back to the default outside home.
        assert_eq!(
            resolve_root(
                Path::new("/"),
                Some(home),
                Path::new("~other/.qmux"),
                Some(default)
            ),
            default.to_path_buf()
        );
        // With no home there is nothing to expand against; the default applies.
        assert_eq!(
            resolve_root(Path::new("/"), None, Path::new("~/.qmux"), Some(default)),
            default.to_path_buf()
        );
    }

    #[test]
    fn relative_root_resolves_against_cwd_inside_home() {
        let home = Path::new("/Users/tester");
        let cwd = Path::new("/Users/tester/Code/project");
        let default = Path::new("/Users/tester/qmux/workspaces");
        assert_eq!(
            resolve_root(
                cwd,
                Some(home),
                Path::new(".qmux/workspaces"),
                Some(default)
            ),
            PathBuf::from("/Users/tester/Code/project/.qmux/workspaces")
        );
    }

    #[test]
    fn relative_root_falls_back_to_default_outside_home() {
        let home = Path::new("/Users/tester");
        let default = Path::new("/Users/tester/qmux/workspaces");
        // Finder/Dock launch: process cwd is the filesystem root.
        assert_eq!(
            resolve_root(
                Path::new("/"),
                Some(home),
                Path::new(".qmux/workspaces"),
                Some(default)
            ),
            PathBuf::from("/Users/tester/qmux/workspaces")
        );
        // Binary launched from a system directory.
        assert_eq!(
            resolve_root(
                Path::new("/usr/local/bin"),
                Some(home),
                Path::new(".qmux/workspaces"),
                Some(default)
            ),
            PathBuf::from("/Users/tester/qmux/workspaces")
        );
    }

    #[test]
    fn absolute_root_is_honored_regardless_of_cwd() {
        let home = Path::new("/Users/tester");
        let default = Path::new("/Users/tester/qmux/workspaces");
        assert_eq!(
            resolve_root(
                Path::new("/"),
                Some(home),
                Path::new("/opt/qmux/ws"),
                Some(default)
            ),
            PathBuf::from("/opt/qmux/ws")
        );
    }

    #[test]
    fn relative_root_outside_home_without_default_uses_cwd() {
        // No home to fall back to: the relative path resolves against cwd rather
        // than being dropped entirely.
        assert_eq!(
            resolve_root(
                Path::new("/srv/app"),
                None,
                Path::new(".qmux/workspaces"),
                None
            ),
            PathBuf::from("/srv/app/.qmux/workspaces")
        );
    }

    #[test]
    fn plugin_dir_override_wins_and_is_absolutized() {
        let cwd = Path::new("/tmp/qmux-cfg");
        // Absolute override is used verbatim.
        assert_eq!(
            pick_claude_plugin_dir(cwd, Some(Path::new("/opt/skills")), None),
            PathBuf::from("/opt/skills")
        );
        // Relative override is resolved against the cwd.
        assert_eq!(
            pick_claude_plugin_dir(cwd, Some(Path::new("rel/skills")), None),
            PathBuf::from("/tmp/qmux-cfg/rel/skills")
        );
    }

    #[test]
    fn plugin_dir_prefers_first_existing_candidate() {
        let base = env::temp_dir().join(format!("qmux-claude-plugindir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let cwd = base.join("cwd");
        let exe_dir = base.join("exe");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&exe_dir).unwrap();

        // Nothing exists yet -> falls back to <cwd>/qmux-claude-plugin.
        assert_eq!(
            pick_claude_plugin_dir(&cwd, None, Some(&exe_dir)),
            cwd.join("qmux-claude-plugin")
        );

        // An exe-adjacent plugin is found even though cwd has none — the case that
        // previously failed when the process cwd was not the repo root.
        let exe_plugin = exe_dir.join("qmux-claude-plugin");
        fs::create_dir_all(&exe_plugin).unwrap();
        assert_eq!(
            pick_claude_plugin_dir(&cwd, None, Some(&exe_dir)),
            exe_plugin
        );

        // The cwd candidate takes precedence once it exists.
        let cwd_plugin = cwd.join("qmux-claude-plugin");
        fs::create_dir_all(&cwd_plugin).unwrap();
        assert_eq!(
            pick_claude_plugin_dir(&cwd, None, Some(&exe_dir)),
            cwd_plugin
        );

        let _ = fs::remove_dir_all(&base);
    }
}
