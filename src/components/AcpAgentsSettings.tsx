import { useCallback, useEffect, useMemo, useState } from "react";
import {
  getRuntimeConfig,
  installAcpRegistryAgent,
  listAcpRegistry,
  setAcpDefaultAgent,
  uninstallAcpRegistryAgent,
} from "../lib/api";
import type { AcpAgentChoice, AcpRegistryEntry, RuntimeConfig } from "../types";

interface AcpAgentsSettingsProps {
  /** Current runtime config; used for the installed list until a local refresh. */
  config: RuntimeConfig | null;
  /** Push a refreshed config into App after install/uninstall/default changes. */
  onConfigChange: (config: RuntimeConfig) => void;
}

function availabilityLabel(entry: AcpRegistryEntry): string {
  if ("available" in entry.availability) {
    return entry.availability.available.channel;
  }
  return entry.availability.unavailable.reason;
}

function isAvailable(entry: AcpRegistryEntry): boolean {
  return "available" in entry.availability;
}

/**
 * Settings → Agents: manage ACP agents from the published registry and the
 * local pin store. Hand-written `qmux.config.json` entries are listed read-only
 * so this surface never rewrites the user's config file.
 */
export default function AcpAgentsSettings({ config, onConfigChange }: AcpAgentsSettingsProps) {
  const [agents, setAgents] = useState<AcpAgentChoice[]>(config?.acpAgents ?? []);
  const [registry, setRegistry] = useState<AcpRegistryEntry[] | null>(null);
  const [filter, setFilter] = useState("");
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    setAgents(config?.acpAgents ?? []);
  }, [config?.acpAgents]);

  const refreshRuntime = useCallback(
    async (choices?: AcpAgentChoice[]) => {
      if (choices) {
        setAgents(choices);
      }
      try {
        const runtime = await getRuntimeConfig();
        setAgents(runtime.acpAgents);
        onConfigChange(runtime);
      } catch (err) {
        // Install already succeeded; a refresh failure only leaves the picker
        // briefly stale until the next boot.
        if (!choices) {
          setError(err instanceof Error ? err.message : String(err));
        }
      }
    },
    [onConfigChange],
  );

  const loadRegistry = useCallback(async (refresh: boolean) => {
    setLoading(true);
    setError(null);
    try {
      const entries = await listAcpRegistry(refresh);
      setRegistry(entries);
      setStatus(refresh ? "Registry refreshed." : null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadRegistry(false);
  }, [loadRegistry]);

  const filteredRegistry = useMemo(() => {
    if (!registry) {
      return [];
    }
    const needle = filter.trim().toLowerCase();
    if (!needle) {
      return registry;
    }
    return registry.filter((entry) => {
      const haystack = [entry.id, entry.name, entry.description ?? "", entry.authors?.join(" ") ?? ""]
        .join(" ")
        .toLowerCase();
      return haystack.includes(needle);
    });
  }, [filter, registry]);

  async function handleInstall(id: string) {
    setBusyId(id);
    setError(null);
    setStatus(null);
    try {
      const choices = await installAcpRegistryAgent(id);
      await refreshRuntime(choices);
      setRegistry((current) =>
        current?.map((entry) => (entry.id === id ? { ...entry, installed: true } : entry)) ?? null,
      );
      setStatus(`Added ${id}.`);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  async function handleUninstall(id: string) {
    setBusyId(id);
    setError(null);
    setStatus(null);
    try {
      const choices = await uninstallAcpRegistryAgent(id);
      await refreshRuntime(choices);
      setRegistry((current) =>
        current?.map((entry) => (entry.id === id ? { ...entry, installed: false } : entry)) ?? null,
      );
      setStatus(`Removed ${id}.`);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  async function handleSetDefault(id: string | null) {
    setBusyId(id ?? "__clear_default__");
    setError(null);
    setStatus(null);
    try {
      const choices = await setAcpDefaultAgent(id);
      await refreshRuntime(choices);
      setStatus(id ? `Default set to ${id}.` : "Default cleared.");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  const configOnly = agents.filter((agent) => !agent.fromRegistry);
  const fromRegistry = agents.filter((agent) => agent.fromRegistry);

  return (
    <div className="acp-agents-settings">
      <section className="acp-agents-section" aria-labelledby="acp-installed-heading">
        <div className="acp-agents-section-header">
          <h3 id="acp-installed-heading">Installed</h3>
        </div>
        {agents.length === 0 ? (
          <p className="settings-hint">None yet. Install one from the registry below.</p>
        ) : (
          <ul className="acp-agents-list">
            {fromRegistry.map((agent) => (
              <li key={agent.id} className="acp-agents-row">
                <div className="acp-agents-row-main">
                  <span className="acp-agents-name">{agent.label}</span>
                  <span className="acp-agents-meta">
                    {agent.id}
                    {agent.default ? " · default" : ""}
                    {" · registry"}
                  </span>
                </div>
                <div className="acp-agents-row-actions">
                  {!agent.default ? (
                    <button
                      type="button"
                      className="control-button"
                      disabled={busyId !== null}
                      onClick={() => void handleSetDefault(agent.id)}
                    >
                      Set default
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="control-button"
                      disabled={busyId !== null}
                      onClick={() => void handleSetDefault(null)}
                    >
                      Clear default
                    </button>
                  )}
                  <button
                    type="button"
                    className="control-button"
                    disabled={busyId !== null}
                    onClick={() => void handleUninstall(agent.id)}
                  >
                    {busyId === agent.id ? "Removing…" : "Remove"}
                  </button>
                </div>
              </li>
            ))}
            {configOnly.map((agent) => (
              <li key={agent.id} className="acp-agents-row">
                <div className="acp-agents-row-main">
                  <span className="acp-agents-name">{agent.label}</span>
                  <span className="acp-agents-meta">
                    {agent.id}
                    {agent.default ? " · default" : ""}
                    {" · qmux.config.json"}
                  </span>
                </div>
                <div className="acp-agents-row-actions">
                  {!agent.default ? (
                    <button
                      type="button"
                      className="control-button"
                      disabled={busyId !== null}
                      onClick={() => void handleSetDefault(agent.id)}
                    >
                      Set default
                    </button>
                  ) : null}
                  <span className="acp-agents-readonly">edit config to change</span>
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="acp-agents-section" aria-labelledby="acp-registry-heading">
        <div className="acp-agents-section-header">
          <h3 id="acp-registry-heading">Registry</h3>
          <button
            type="button"
            className="control-button"
            disabled={loading || busyId !== null}
            onClick={() => void loadRegistry(true)}
          >
            {loading ? "Loading…" : "Refresh"}
          </button>
        </div>
        <input
          type="search"
          className="settings-select acp-agents-filter"
          placeholder="Filter agents…"
          value={filter}
          onChange={(event) => setFilter(event.currentTarget.value)}
          aria-label="Filter registry agents"
        />
        {registry === null && loading ? (
          <p className="settings-hint">Loading registry…</p>
        ) : filteredRegistry.length === 0 ? (
          <p className="settings-hint acp-agents-empty">No agents match.</p>
        ) : (
          <ul className="acp-agents-list acp-agents-list--registry">
            {filteredRegistry.map((entry) => {
              const available = isAvailable(entry);
              return (
                <li key={entry.id} className="acp-agents-row">
                  <div className="acp-agents-row-main">
                    <span className="acp-agents-name">{entry.name}</span>
                    <span className="acp-agents-meta">
                      {entry.id}
                      {entry.version ? ` · v${entry.version}` : ""}
                      {" · "}
                      {available ? availabilityLabel(entry) : "unavailable"}
                    </span>
                    {entry.description ? (
                      <span className="acp-agents-description">{entry.description}</span>
                    ) : null}
                    {!available ? (
                      <span className="acp-agents-unavailable">{availabilityLabel(entry)}</span>
                    ) : null}
                  </div>
                  <div className="acp-agents-row-actions">
                    {entry.installed ? (
                      <button
                        type="button"
                        className="control-button"
                        disabled={busyId !== null}
                        onClick={() => void handleUninstall(entry.id)}
                      >
                        {busyId === entry.id ? "Removing…" : "Remove"}
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="control-button"
                        disabled={!available || busyId !== null}
                        title={!available ? availabilityLabel(entry) : undefined}
                        onClick={() => void handleInstall(entry.id)}
                      >
                        {busyId === entry.id ? "Adding…" : "Add"}
                      </button>
                    )}
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {error ? (
        <p className="settings-hint acp-agents-error" role="alert">
          {error}
        </p>
      ) : null}
      {status && !error ? <p className="settings-hint">{status}</p> : null}
    </div>
  );
}
