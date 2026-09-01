import type { RemoteChoice } from "../types";

export interface RemoteSettingsDraft {
  id: string;
  label: string;
  host: string;
  workspaceRoot: string;
  qmuxCli: string;
  multiplexer: RemoteChoice["multiplexer"];
}

export function remoteIdFromLabel(label: string): string {
  return label
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
}

export function availableRemoteId(seed: string, remotes: RemoteChoice[]): string {
  const taken = new Set(remotes.map((remote) => remote.id));
  const base = remoteIdFromLabel(seed) || "remote";
  if (!taken.has(base)) {
    return base;
  }
  for (let suffix = 2; ; suffix += 1) {
    const ending = `-${suffix}`;
    const candidate = `${base.slice(0, 64 - ending.length).replace(/-+$/g, "")}${ending}`;
    if (!taken.has(candidate)) {
      return candidate;
    }
  }
}

export function remoteDraftFromSshAlias(
  alias: string,
  remotes: RemoteChoice[],
): RemoteSettingsDraft {
  return {
    id: availableRemoteId(alias, remotes),
    label: alias,
    host: alias,
    workspaceRoot: "",
    qmuxCli: "",
    multiplexer: "tmux",
  };
}

export function unconfiguredSshAliases(
  aliases: string[],
  remotes: RemoteChoice[],
): string[] {
  const configuredHosts = new Set<string>();
  for (const remote of remotes) {
    const host = remote.host.trim().toLowerCase();
    configuredHosts.add(host);
    const at = host.lastIndexOf("@");
    if (at >= 0) {
      configuredHosts.add(host.slice(at + 1));
    }
  }
  return aliases.filter((alias) => !configuredHosts.has(alias.trim().toLowerCase()));
}
