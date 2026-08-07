//! Discovery of ACP agents through the published registry.
//!
//! An ACP agent is a command line, so the only thing standing between "this
//! agent exists" and "qmux can run it" is knowing which command line. The
//! registry publishes exactly that: a CDN-hosted index where each agent carries
//! a `distribution` block naming how to launch it.
//!
//! Scope: **package channels only** — `npx` and `uvx`, which need no install
//! because the package manager fetches on demand. The `binary` channel means
//! downloading and executing a third-party archive, and the registry only
//! carries a `sha256` for about half of those entries, so it is reported as
//! unsupported rather than half-implemented.
//!
//! Installed agents land in a qmux-managed store rather than in
//! `qmux.config.json`: that file is hand-written and qmux has never written to
//! it, so a picker must not start rewriting it under the user. The two sources
//! are merged at launch, and a hand-written entry wins any id collision.

use crate::adapters::ensure_on_path;
use crate::config::AcpAgentConfig;
use crate::persistence::STATE_DIR;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
/// How long a cached index is served before a refetch. The registry moves at
/// the pace of agent releases, and a stale entry only means a slightly old
/// pinned version, so this favours not hammering the CDN on every open.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const REGISTRY_CACHE_FILE: &str = "acp-registry.json";
const INSTALLED_FILE: &str = "acp-agents.json";

// ---------------------------------------------------------------------------
// The published index
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RegistryIndex {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAgent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default)]
    pub distribution: Distribution,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Distribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<PackageChannel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<PackageChannel>,
    /// Present but deliberately unmodelled: qmux does not install binaries, and
    /// keeping this as a bare flag avoids implying support it doesn't have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackageChannel {
    /// Version-pinned by the registry, e.g. `cline@3.0.51`.
    pub package: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// What qmux can do with a registry entry, as shown in the picker.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    /// Launchable now; `channel` is `npx` or `uvx`.
    Available { channel: String },
    /// Known but not runnable here, with the reason to show the user.
    Unavailable { reason: String },
}

/// A registry entry decorated with whether qmux can actually run it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    #[serde(flatten)]
    pub agent: RegistryAgent,
    pub availability: Availability,
    /// True when this id is already in the installed store.
    pub installed: bool,
}

/// Resolves an entry to the command line qmux would run.
///
/// `npx` is preferred over `uvx` purely because far more agents publish it;
/// both are equivalent in that the package manager handles fetching.
pub fn resolve_launch(agent: &RegistryAgent) -> Result<(String, AcpAgentConfig), String> {
    let (channel, package, mut args, env) = if let Some(npx) = &agent.distribution.npx {
        // `-y` so a first run doesn't stall on npx's install prompt with no tty
        // attached to answer it.
        ("npx", npx, vec!["-y".to_string()], npx.env.clone())
    } else if let Some(uvx) = &agent.distribution.uvx {
        ("uvx", uvx, Vec::new(), uvx.env.clone())
    } else if agent.distribution.binary.is_some() {
        return Err(
            "this agent ships as a prebuilt binary, which qmux does not install yet".to_string(),
        );
    } else {
        return Err("this agent publishes no distribution qmux understands".to_string());
    };

    // The runner has to exist before an agent using it can be offered.
    // Discovering this at launch instead would mean a dead pane and a much
    // worse error.
    if ensure_on_path(channel).is_none() {
        return Err(format!(
            "'{channel}' was not found on PATH; install it to run this agent"
        ));
    }

    args.push(package.package.clone());
    args.extend(package.args.iter().cloned());
    Ok((
        channel.to_string(),
        AcpAgentConfig {
            name: Some(agent.name.clone()),
            command: channel.to_string(),
            args,
            env,
        },
    ))
}

pub fn describe(agent: &RegistryAgent, installed: bool) -> RegistryEntry {
    let availability = match resolve_launch(agent) {
        Ok((channel, _)) => Availability::Available { channel },
        Err(reason) => Availability::Unavailable { reason },
    };
    RegistryEntry {
        agent: agent.clone(),
        availability,
        installed,
    }
}

// ---------------------------------------------------------------------------
// The installed store
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledAgents {
    #[serde(default)]
    pub agents: BTreeMap<String, InstalledAgent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledAgent {
    pub config: AcpAgentConfig,
    /// Recorded so a later "why is this running an old version" is answerable,
    /// and so a refreshed registry can be diffed against what is pinned here.
    pub registry_version: String,
    pub channel: String,
    pub added_at: u128,
}

pub fn installed_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(STATE_DIR).join(INSTALLED_FILE)
}

/// Reads the store. A missing file is an empty store; a *damaged* one is an
/// error, so a parse failure can't silently drop every agent the user added.
pub fn load_installed(workspace_root: &Path) -> Result<InstalledAgents, String> {
    let path = installed_path(workspace_root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Default::default());
    }
    serde_json::from_str(&raw).map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

/// Serializes installs so two in flight at once can't each read the same
/// snapshot and then overwrite the other's agent — the same read-modify-write
/// hazard `persistence` guards preferences against.
static STORE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn save_installed(workspace_root: &Path, installed: &InstalledAgents) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(installed)
        .map_err(|err| format!("failed to encode ACP agents: {err}"))?;
    write_json_atomic(&installed_path(workspace_root), &raw)
}

/// Adds `agent` to the store, replacing any previous pin for the same id.
pub fn install(
    workspace_root: &Path,
    agent: &RegistryAgent,
    registry_version: &str,
) -> Result<InstalledAgent, String> {
    let (channel, config) = resolve_launch(agent)?;
    let record = InstalledAgent {
        config,
        registry_version: registry_version.to_string(),
        channel,
        added_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default(),
    };
    let _guard = STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut installed = load_installed(workspace_root)?;
    installed.agents.insert(agent.id.clone(), record.clone());
    save_installed(workspace_root, &installed)?;
    Ok(record)
}

pub fn uninstall(workspace_root: &Path, id: &str) -> Result<bool, String> {
    let _guard = STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut installed = load_installed(workspace_root)?;
    let removed = installed.agents.remove(id).is_some();
    if removed {
        save_installed(workspace_root, &installed)?;
    }
    Ok(removed)
}

/// The installed agents as the adapter consumes them. Propagates a damaged
/// store rather than reporting "no agents": silently launching as if the file
/// were empty would read as the agents having vanished.
pub fn installed_configs(
    workspace_root: &Path,
) -> Result<BTreeMap<String, AcpAgentConfig>, String> {
    Ok(load_installed(workspace_root)?
        .agents
        .into_iter()
        .map(|(id, agent)| (id, agent.config))
        .collect())
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

fn cache_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(STATE_DIR).join(REGISTRY_CACHE_FILE)
}

fn cached_index(workspace_root: &Path, max_age: Duration) -> Option<RegistryIndex> {
    let path = cache_path(workspace_root);
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > max_age {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()
}

/// Returns the registry, preferring a fresh cache. `force` skips the cache.
///
/// A failed fetch falls back to *any* cached copy regardless of age: an old
/// agent list is far more useful than an empty picker when the network is down.
pub async fn fetch_index(workspace_root: &Path, force: bool) -> Result<RegistryIndex, String> {
    if !force && let Some(cached) = cached_index(workspace_root, CACHE_TTL) {
        return Ok(cached);
    }

    match download_index().await {
        Ok((index, raw)) => {
            let _ = write_json(&cache_path(workspace_root), &raw);
            Ok(index)
        }
        Err(err) => cached_index(workspace_root, Duration::MAX)
            .ok_or_else(|| format!("could not reach the ACP registry: {err}")),
    }
}

async fn download_index() -> Result<(RegistryIndex, String), String> {
    crate::ensure_rustls_crypto_provider()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|err| format!("failed to build the registry HTTP client: {err}"))?;
    let response = client
        .get(REGISTRY_URL)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("registry returned {}", response.status()));
    }
    let raw = response
        .text()
        .await
        .map_err(|err| format!("failed to read the registry response: {err}"))?;
    let index: RegistryIndex =
        serde_json::from_str(&raw).map_err(|err| format!("registry is not valid JSON: {err}"))?;
    Ok((index, raw))
}

/// A plain write, used for the registry cache — a torn cache is re-fetched, so
/// it does not need the ceremony below.
fn write_json(path: &Path, raw: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(path, raw).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

/// Writes through a temp file and renames over the target. The installed store
/// is the only copy of what the user added, and `load_installed` refuses to
/// parse a damaged file, so a crash mid-write would otherwise leave them with a
/// hard error and no agents.
fn write_json_atomic(path: &Path, raw: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, raw).map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {err}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{self, AtomicU64};

    fn agent(distribution: serde_json::Value) -> RegistryAgent {
        serde_json::from_value(json!({
            "id": "demo", "name": "Demo", "version": "1.2.3",
            "distribution": distribution,
        }))
        .expect("fixture parses")
    }

    /// A directory no other test can be handed. Tests run in parallel threads
    /// and the clock is not fine-grained enough to separate them on its own, so
    /// a counter — not a timestamp — is what makes this unique.
    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "qmux-acp-registry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn an_npx_agent_resolves_to_a_pinned_npx_invocation() {
        let agent = agent(json!({ "npx": {
            "package": "cline@3.0.51",
            "args": ["--acp"],
            "env": { "CLINE_DISABLE_AUTO_UPDATE": "1" },
        }}));
        let (channel, config) = resolve_launch(&agent).expect("npx is available in test envs");

        assert_eq!(channel, "npx");
        assert_eq!(config.command, "npx");
        // `-y` first, then the version-pinned package, then the agent's flags.
        assert_eq!(config.args, ["-y", "cline@3.0.51", "--acp"]);
        assert_eq!(config.name.as_deref(), Some("Demo"));
        assert_eq!(config.env["CLINE_DISABLE_AUTO_UPDATE"], "1");
    }

    #[test]
    fn a_uvx_agent_resolves_without_the_npm_only_flag() {
        let agent =
            agent(json!({ "uvx": { "package": "fast-agent-acp==0.9.30", "args": ["-x"] } }));
        let Ok((channel, config)) = resolve_launch(&agent) else {
            // uvx is not universally installed; skip rather than fail the suite.
            return;
        };
        assert_eq!(channel, "uvx");
        assert_eq!(config.args, ["fast-agent-acp==0.9.30", "-x"]);
    }

    #[test]
    fn npx_is_preferred_when_an_agent_publishes_several_channels() {
        let agent = agent(json!({
            "npx": { "package": "thing@1" },
            "uvx": { "package": "thing==1" },
            "binary": { "darwin-aarch64": { "archive": "…", "cmd": "./thing" } },
        }));
        assert_eq!(resolve_launch(&agent).unwrap().0, "npx");
    }

    #[test]
    fn binary_only_agents_are_reported_as_unsupported_rather_than_missing() {
        let agent = agent(json!({ "binary": {
            "darwin-aarch64": { "archive": "https://example/x.tar.gz", "cmd": "./x" },
        }}));
        let err = resolve_launch(&agent).expect_err("binary is out of scope");
        assert!(err.contains("prebuilt binary"), "{err}");

        // The picker still lists it, with the reason attached.
        let entry = describe(&agent, false);
        match entry.availability {
            Availability::Unavailable { reason } => assert!(reason.contains("binary"), "{reason}"),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn an_agent_with_no_usable_distribution_says_so() {
        let err = resolve_launch(&agent(json!({}))).expect_err("nothing to run");
        assert!(err.contains("no distribution"), "{err}");
    }

    #[test]
    fn the_real_registry_payload_parses_and_mostly_resolves() {
        // A trimmed copy of the published index, verbatim in shape.
        let index: RegistryIndex = serde_json::from_value(json!({
            "version": "1.0.0",
            "agents": [
                { "id": "cline", "name": "Cline", "version": "3.0.51",
                  "description": "…", "repository": "https://example",
                  "authors": ["Cline"], "license": "Apache-2.0",
                  "icon": "https://cdn.example/cline.svg",
                  "distribution": { "npx": { "package": "cline@3.0.51", "args": ["--acp"] } } },
                { "id": "goose", "name": "goose", "version": "1.45.0",
                  "distribution": { "binary": { "darwin-aarch64": {
                      "archive": "https://example/goose.tar.bz2", "cmd": "./goose",
                      "args": ["acp"], "sha256": "abc" } } } },
                { "id": "fast-agent", "name": "fast-agent", "version": "0.9.30",
                  "distribution": { "uvx": { "package": "fast-agent-acp==0.9.30", "args": ["-x"] } } },
            ],
        }))
        .expect("the published shape parses");

        assert_eq!(index.version, "1.0.0");
        assert_eq!(index.agents.len(), 3);
        assert_eq!(index.agents[0].authors, ["Cline"]);
        assert!(resolve_launch(&index.agents[0]).is_ok());
        assert!(resolve_launch(&index.agents[1]).is_err(), "binary-only");
    }

    #[test]
    fn unknown_registry_fields_do_not_break_parsing() {
        // The index is versioned and will grow fields; a new one must not blank
        // the whole picker.
        let index: RegistryIndex = serde_json::from_value(json!({
            "version": "1.1.0",
            "agents": [{ "id": "x", "name": "X", "version": "1",
                         "somethingNew": { "nested": true },
                         "distribution": { "npx": { "package": "x@1" }, "future": {} } }],
            "extensions": [],
        }))
        .expect("forward compatible");
        assert_eq!(index.agents.len(), 1);
    }

    #[test]
    fn the_entry_sent_to_the_frontend_has_the_documented_shape() {
        // Serde renames enum *variants* with `rename_all`, not the fields
        // inside them, so pin the exact JSON rather than assuming.
        let entry = describe(
            &agent(json!({ "npx": { "package": "cline@3.0.51" } })),
            true,
        );
        let encoded = serde_json::to_value(&entry).expect("serializes");

        // `agent` is flattened, so its fields sit at the top level.
        assert_eq!(encoded["id"], "demo");
        assert_eq!(encoded["version"], "1.2.3");
        assert_eq!(encoded["installed"], true);
        assert_eq!(
            encoded["availability"],
            json!({ "available": { "channel": "npx" } })
        );

        let unavailable = describe(&agent(json!({})), false);
        let encoded = serde_json::to_value(&unavailable).expect("serializes");
        assert_eq!(
            encoded["availability"]["unavailable"]["reason"],
            "this agent publishes no distribution qmux understands"
        );
    }

    #[test]
    fn installing_pins_the_resolved_command_and_survives_a_reload() {
        let root = scratch();
        let agent = agent(json!({ "npx": { "package": "cline@3.0.51", "args": ["--acp"] } }));

        let record = install(&root, &agent, "1.0.0").expect("installs");
        assert_eq!(record.channel, "npx");
        assert_eq!(record.registry_version, "1.0.0");
        assert_eq!(record.config.args, ["-y", "cline@3.0.51", "--acp"]);

        let configs = installed_configs(&root).expect("store reads");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs["demo"].command, "npx");

        assert!(uninstall(&root, "demo").expect("removes"));
        assert!(installed_configs(&root).expect("store reads").is_empty());
        // Removing something absent is not an error, just `false`.
        assert!(!uninstall(&root, "demo").expect("no-op"));
    }

    #[test]
    fn reinstalling_replaces_the_previous_pin() {
        let root = scratch();
        install(
            &root,
            &agent(json!({ "npx": { "package": "cline@3.0.0" } })),
            "1.0.0",
        )
        .expect("installs");
        install(
            &root,
            &agent(json!({ "npx": { "package": "cline@3.0.51" } })),
            "1.1.0",
        )
        .expect("reinstalls");

        let configs = installed_configs(&root).expect("store reads");
        assert_eq!(configs.len(), 1, "the id is the key, not the version");
        assert_eq!(configs["demo"].args, ["-y", "cline@3.0.51"]);
    }

    #[test]
    fn a_missing_store_is_empty_but_a_damaged_one_is_an_error() {
        let root = scratch();
        assert!(
            load_installed(&root)
                .expect("missing is empty")
                .agents
                .is_empty()
        );

        let path = installed_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ this is not json").unwrap();
        // Silently treating a damaged file as empty would erase the user's
        // agents on the next write.
        assert!(load_installed(&root).is_err());
    }

    #[test]
    fn the_cache_is_served_while_fresh_and_ignored_once_stale() {
        let root = scratch();
        let raw = json!({ "version": "1.0.0", "agents": [] }).to_string();
        write_json(&cache_path(&root), &raw).expect("writes cache");

        assert!(cached_index(&root, Duration::from_secs(3600)).is_some());
        // A zero TTL makes any cache stale, which is what `force` relies on.
        assert!(cached_index(&root, Duration::ZERO).is_none());
    }
}
