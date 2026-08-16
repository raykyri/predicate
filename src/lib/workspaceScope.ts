import type { GroupInfo, PaneInfo, ResearchTreeSummary } from "../types";

export type WorkspaceScope = GroupInfo["scope"];

export function groupsForScope(groups: GroupInfo[], scope: WorkspaceScope): GroupInfo[] {
  return groups.filter((group) => group.scope === scope);
}

export function paneCanOpenWorktree(
  agent: { activeWorkspace?: { gitRoot?: string | null } | null } | undefined,
  group: GroupInfo | undefined,
): { enabled: boolean; reason?: string } {
  if (group?.scope === "research") {
    return { enabled: false, reason: "Worktrees are not available in Research" };
  }
  if (agent?.activeWorkspace && !agent.activeWorkspace.gitRoot) {
    return { enabled: false, reason: "This tab is not inside a git repository" };
  }
  return { enabled: true };
}

export function paneScope(
  pane: PaneInfo,
  groupById: ReadonlyMap<string, GroupInfo>,
): WorkspaceScope {
  // A pane without a valid group is legacy/corrupt state. Keep it reachable in
  // Terminal mode so recovery never hides the only route to the process.
  return groupById.get(pane.groupId)?.scope ?? "terminal";
}

export function panesForScope(
  panes: PaneInfo[],
  groups: GroupInfo[],
  scope: WorkspaceScope,
): PaneInfo[] {
  const scopedGroups = groupsForScope(groups, scope);
  const groupById = new Map(groups.map((group) => [group.id, group]));
  const grouped = scopedGroups.flatMap((group) =>
    panes.filter((pane) => pane.groupId === group.id),
  );
  const groupedIds = new Set(grouped.map((pane) => pane.id));
  const ungrouped = panes.filter(
    (pane) => !groupedIds.has(pane.id) && paneScope(pane, groupById) === scope,
  );
  return [...grouped, ...ungrouped];
}

export function researchAttention(trees: ResearchTreeSummary[]) {
  return trees.reduce(
    (attention, tree) => ({
      runningCount: attention.runningCount + tree.runningCount,
      // Unseen failures, not the lifetime failedCount: one flaky follow-up
      // must not brand the Research toggle with "!" forever — viewing the
      // tree acknowledges it.
      failedCount: attention.failedCount + Number(tree.hasUnseenFailure),
      unseenCount: attention.unseenCount + Number(tree.hasUnseenUpdate),
    }),
    { runningCount: 0, failedCount: 0, unseenCount: 0 },
  );
}

/** Reorders one scope while preserving the other scope's slots and identity. */
export function replaceScopedGroupOrder(
  groups: GroupInfo[],
  scope: WorkspaceScope,
  orderedScopeGroups: GroupInfo[],
): GroupInfo[] {
  const expected = groupsForScope(groups, scope);
  if (
    expected.length !== orderedScopeGroups.length ||
    new Set(orderedScopeGroups.map((group) => group.id)).size !== orderedScopeGroups.length ||
    orderedScopeGroups.some(
      (group) => group.scope !== scope || !expected.some((candidate) => candidate.id === group.id),
    )
  ) {
    return groups;
  }
  let index = 0;
  return groups.map((group) =>
    group.scope === scope ? orderedScopeGroups[index++] : group,
  );
}
