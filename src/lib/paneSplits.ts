import type {
  PaneInfo,
  PaneSplitAxis,
  PaneSplitInfo,
  PaneSplitIntent,
  PaneSplitIntentPosition,
  PaneSplitIntentSource,
} from "../types";

const MIN_SPLIT_FRACTION = 0.12;
const INSERTED_RELATIVE_INTENT_KIND = "inserted-relative";
const VALID_INTENT_SOURCES = new Set<PaneSplitIntentSource>([
  "command",
  "join",
  "drag-half",
  "drag-divider",
]);
const VALID_INTENT_POSITIONS = new Set<PaneSplitIntentPosition>(["above", "below"]);

interface JoinPaneSplitOptions {
  source?: PaneSplitIntentSource;
  insertedPaneId?: string;
  createdAt?: number;
}

function panePositions(panes: PaneInfo[]) {
  const groupIndexes = new Map<string, number>();
  const positions = new Map<string, { groupId: string; index: number }>();
  for (const pane of panes) {
    const index = groupIndexes.get(pane.groupId) ?? 0;
    positions.set(pane.id, { groupId: pane.groupId, index });
    groupIndexes.set(pane.groupId, index + 1);
  }
  return positions;
}

function orderedContiguousPaneIds(panes: PaneInfo[], paneIds: Iterable<string>): string[] | null {
  const positions = panePositions(panes);
  const ids = [...new Set(paneIds)];
  if (ids.length < 2) {
    return null;
  }
  const first = positions.get(ids[0]);
  if (!first) {
    return null;
  }
  if (ids.some((id) => positions.get(id)?.groupId !== first.groupId)) {
    return null;
  }
  ids.sort((a, b) => (positions.get(a)?.index ?? 0) - (positions.get(b)?.index ?? 0));
  for (let index = 1; index < ids.length; index += 1) {
    const previous = positions.get(ids[index - 1]);
    const current = positions.get(ids[index]);
    if (!previous || !current || current.index !== previous.index + 1) {
      return null;
    }
  }
  return ids;
}

function splitIdFor(paneIds: string[]) {
  return `split-${paneIds.join("-")}`;
}

function equalSizes(paneIds: string[]) {
  const size = paneIds.length > 0 ? 1 / paneIds.length : 1;
  return Object.fromEntries(paneIds.map((paneId) => [paneId, size]));
}

function normalizedSizesForPaneIds(split: PaneSplitInfo, paneIds: string[]) {
  const raw = paneIds.map((paneId) => split.sizes?.[paneId] ?? 0);
  const total = raw.reduce(
    (sum, value) => sum + (Number.isFinite(value) && value > 0 ? value : 0),
    0,
  );
  if (total <= 0) {
    return equalSizes(paneIds);
  }
  return Object.fromEntries(
    paneIds.map((paneId, index) => [
      paneId,
      Number.isFinite(raw[index]) && raw[index] > 0 ? raw[index] / total : 0,
    ]),
  );
}

function isValidPaneSplitIntent(value: unknown, paneIdSet: Set<string>): value is PaneSplitIntent {
  const intent = value as Partial<PaneSplitIntent> | null;
  return (
    Boolean(intent) &&
    intent?.kind === INSERTED_RELATIVE_INTENT_KIND &&
    typeof intent.anchorPaneId === "string" &&
    paneIdSet.has(intent.anchorPaneId) &&
    typeof intent.position === "string" &&
    VALID_INTENT_POSITIONS.has(intent.position as PaneSplitIntentPosition) &&
    typeof intent.source === "string" &&
    VALID_INTENT_SOURCES.has(intent.source as PaneSplitIntentSource) &&
    typeof intent.createdAt === "number" &&
    Number.isFinite(intent.createdAt) &&
    intent.createdAt >= 0
  );
}

function normalizedIntentForPaneIds(
  split: PaneSplitInfo,
  paneIds: string[],
): Record<string, PaneSplitIntent> | undefined {
  const paneIdSet = new Set(paneIds);
  const entries = Object.entries(split.intent ?? {}).filter(
    ([paneId, intent]) =>
      paneIdSet.has(paneId) &&
      isValidPaneSplitIntent(intent, paneIdSet) &&
      intent.anchorPaneId !== paneId,
  );
  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

function joinedPaneSizes(existingSplits: PaneSplitInfo[], paneIds: string[]) {
  const paneIdSet = new Set(paneIds);
  const weights = new Map<string, number>();

  for (const split of existingSplits) {
    const splitPaneIds = split.paneIds.filter((paneId) => paneIdSet.has(paneId));
    if (splitPaneIds.length === 0) {
      continue;
    }
    const sizes = normalizedSizesForPaneIds(split, splitPaneIds);
    for (const paneId of splitPaneIds) {
      if (!weights.has(paneId)) {
        weights.set(paneId, (sizes[paneId] ?? 0) * splitPaneIds.length);
      }
    }
  }

  for (const paneId of paneIds) {
    if (!weights.has(paneId)) {
      weights.set(paneId, 1);
    }
  }

  const total = [...weights.values()].reduce(
    (sum, value) => sum + (Number.isFinite(value) && value > 0 ? value : 0),
    0,
  );
  if (total <= 0) {
    return equalSizes(paneIds);
  }
  return Object.fromEntries(
    paneIds.map((paneId) => [paneId, (weights.get(paneId) ?? 0) / total]),
  );
}

function insertedPaneIntent(
  paneIds: string[],
  paneId: string,
  belowPaneId: string,
  options: JoinPaneSplitOptions,
): [string, PaneSplitIntent] | null {
  if (!options.insertedPaneId || !paneIds.includes(options.insertedPaneId)) {
    return null;
  }

  const source = options.source ?? "join";
  const createdAt = options.createdAt ?? Date.now();
  if (!VALID_INTENT_SOURCES.has(source) || !Number.isFinite(createdAt) || createdAt < 0) {
    return null;
  }

  let anchorPaneId: string | null = null;
  let position: PaneSplitIntentPosition | null = null;
  if (options.insertedPaneId === paneId && paneIds.includes(belowPaneId)) {
    anchorPaneId = belowPaneId;
    position = "above";
  } else if (options.insertedPaneId === belowPaneId && paneIds.includes(paneId)) {
    anchorPaneId = paneId;
    position = "below";
  } else {
    const index = paneIds.indexOf(options.insertedPaneId);
    if (index > 0) {
      anchorPaneId = paneIds[index - 1];
      position = "below";
    } else if (index >= 0 && index < paneIds.length - 1) {
      anchorPaneId = paneIds[index + 1];
      position = "above";
    }
  }

  if (!anchorPaneId || !position || anchorPaneId === options.insertedPaneId) {
    return null;
  }

  return [
    options.insertedPaneId,
    {
      kind: INSERTED_RELATIVE_INTENT_KIND,
      anchorPaneId,
      position,
      source,
      createdAt,
    },
  ];
}

function joinedPaneIntent(
  existingSplits: PaneSplitInfo[],
  paneIds: string[],
  paneId: string,
  belowPaneId: string,
  options: JoinPaneSplitOptions,
): Record<string, PaneSplitIntent> | undefined {
  const paneIdSet = new Set(paneIds);
  const intent: Record<string, PaneSplitIntent> = {};

  for (const split of existingSplits) {
    const existingIntent = normalizedIntentForPaneIds(
      split,
      split.paneIds.filter((candidate) => paneIdSet.has(candidate)),
    );
    for (const [intentPaneId, entry] of Object.entries(existingIntent ?? {})) {
      if (!intent[intentPaneId]) {
        intent[intentPaneId] = entry;
      }
    }
  }

  const existingPaneIds = new Set(existingSplits.flatMap((split) => split.paneIds));
  const inserted = insertedPaneIntent(paneIds, paneId, belowPaneId, options);
  if (inserted && !existingPaneIds.has(inserted[0])) {
    intent[inserted[0]] = inserted[1];
  }

  const normalized = normalizedIntentForPaneIds(
    {
      id: "joined-intent",
      paneIds,
      sizes: {},
      intent,
    },
    paneIds,
  );
  return normalized;
}

export function normalizePaneSplitsForPanes(
  splits: PaneSplitInfo[],
  panes: PaneInfo[],
): PaneSplitInfo[] {
  const availablePaneIds = new Set(panes.map((pane) => pane.id));
  const used = new Set<string>();
  const usedSplitIds = new Set<string>();
  const normalized: PaneSplitInfo[] = [];

  for (const split of splits) {
    if (!split.id || usedSplitIds.has(split.id)) {
      continue;
    }
    const paneIds = orderedContiguousPaneIds(
      panes,
      split.paneIds.filter((paneId) => availablePaneIds.has(paneId) && !used.has(paneId)),
    );
    if (!paneIds) {
      continue;
    }
    for (const paneId of paneIds) {
      used.add(paneId);
    }
    usedSplitIds.add(split.id);
    const normalizedSplit: PaneSplitInfo = {
      id: split.id,
      paneIds,
      sizes: Object.fromEntries(
        Object.entries(split.sizes ?? {}).filter(
          ([paneId, size]) => paneIds.includes(paneId) && Number.isFinite(size) && size > 0,
        ),
      ),
    };
    if (paneSplitAxis(split) === "horizontal") {
      normalizedSplit.axis = "horizontal";
    }
    const intent = normalizedIntentForPaneIds(split, paneIds);
    if (intent) {
      normalizedSplit.intent = intent;
    }
    normalized.push(normalizedSplit);
  }

  return normalized;
}

export function paneSplitAxis(split: PaneSplitInfo | null | undefined): PaneSplitAxis {
  return split?.axis === "horizontal" ? "horizontal" : "vertical";
}

export function withPaneSplitAxis(split: PaneSplitInfo, axis: PaneSplitAxis): PaneSplitInfo {
  if (axis === "horizontal") {
    return split.axis === "horizontal" ? split : { ...split, axis: "horizontal" };
  }
  if (split.axis !== "horizontal") {
    return split;
  }
  const next = { ...split };
  delete next.axis;
  return next;
}

export function togglePaneSplitAxis(split: PaneSplitInfo): PaneSplitInfo {
  return withPaneSplitAxis(
    split,
    paneSplitAxis(split) === "horizontal" ? "vertical" : "horizontal",
  );
}

export function paneSplitForPane(splits: PaneSplitInfo[], paneId: string | null | undefined) {
  if (!paneId) {
    return null;
  }
  return splits.find((split) => split.paneIds.includes(paneId)) ?? null;
}

export function paneSplitFlagIsEnabled(
  flagsByPane: Readonly<Record<string, boolean>>,
  paneIds: readonly string[],
) {
  return paneIds.some((paneId) => flagsByPane[paneId] === true);
}

export function setPaneSplitFlagEnabled(
  flagsByPane: Record<string, boolean>,
  paneIds: readonly string[],
  enabled: boolean,
) {
  let next: Record<string, boolean> | null = null;
  for (const paneId of paneIds) {
    if (enabled) {
      if (flagsByPane[paneId] === true) {
        continue;
      }
      next ??= { ...flagsByPane };
      next[paneId] = true;
      continue;
    }
    if (!Object.prototype.hasOwnProperty.call(flagsByPane, paneId)) {
      continue;
    }
    next ??= { ...flagsByPane };
    delete next[paneId];
  }
  return next ?? flagsByPane;
}

export function paneSnapshotForPersistedPaneSplits(
  persistedSplits: PaneSplitInfo[],
  currentPanes: PaneInfo[],
  requestedPanes: PaneInfo[],
) {
  const persistedPaneIds = new Set(persistedSplits.flatMap((split) => split.paneIds));
  if (persistedPaneIds.size === 0) {
    return currentPanes;
  }

  const currentPaneIds = new Set(currentPanes.map((pane) => pane.id));
  if ([...persistedPaneIds].every((paneId) => currentPaneIds.has(paneId))) {
    return currentPanes;
  }

  const requestedPaneIds = new Set(requestedPanes.map((pane) => pane.id));
  return [...persistedPaneIds].every((paneId) => requestedPaneIds.has(paneId))
    ? requestedPanes
    : currentPanes;
}

export function adjacentPaneBelow(panes: PaneInfo[], pane: PaneInfo | null | undefined) {
  if (!pane) {
    return null;
  }
  const groupPanes = panes.filter((candidate) => candidate.groupId === pane.groupId);
  const index = groupPanes.findIndex((candidate) => candidate.id === pane.id);
  return index >= 0 ? (groupPanes[index + 1] ?? null) : null;
}

export function joinPaneSplit(
  splits: PaneSplitInfo[],
  panes: PaneInfo[],
  paneId: string,
  belowPaneId: string,
  options: JoinPaneSplitOptions = {},
): PaneSplitInfo[] {
  const normalized = normalizePaneSplitsForPanes(splits, panes);
  // Use the raw split membership here, not only the already-normalized groups.
  // `Split terminal` inserts a new tab between existing split members, so the old
  // group can be temporarily non-contiguous in the new tab order until this merge
  // builds the replacement group.
  const existing = splits.filter(
    (split) => split.paneIds.includes(paneId) || split.paneIds.includes(belowPaneId),
  );
  const paneIds = orderedContiguousPaneIds(
    panes,
    existing.flatMap((split) => split.paneIds).concat([paneId, belowPaneId]),
  );
  if (!paneIds) {
    return normalized;
  }
  const id = existing[0]?.id ?? splitIdFor(paneIds);
  const existingPaneIds = new Set(existing.flatMap((split) => split.paneIds));
  const existingSplitIds = new Set(existing.map((split) => split.id));
  const joinedSplit: PaneSplitInfo = {
    id,
    paneIds,
    sizes: joinedPaneSizes(existing, paneIds),
  };
  const axisSource =
    existing.find((split) => split.paneIds.includes(paneId)) ?? existing[0];
  if (paneSplitAxis(axisSource) === "horizontal") {
    joinedSplit.axis = "horizontal";
  }
  const intent = joinedPaneIntent(existing, paneIds, paneId, belowPaneId, options);
  if (intent) {
    joinedSplit.intent = intent;
  }

  return [
    ...normalized.filter(
      (split) =>
        !existingSplitIds.has(split.id) &&
        !split.paneIds.some((paneId) => existingPaneIds.has(paneId)),
    ),
    joinedSplit,
  ];
}

export function detachPaneFromSplitMemberships(
  splits: PaneSplitInfo[],
  paneId: string,
): PaneSplitInfo[] {
  return splits
    .map((split) => {
      if (!split.paneIds.includes(paneId)) {
        return split;
      }
      const paneIds = split.paneIds.filter((id) => id !== paneId);
      const nextSplit: PaneSplitInfo = {
        ...split,
        paneIds,
        sizes: Object.fromEntries(
          Object.entries(split.sizes ?? {}).filter(([id]) => id !== paneId),
        ),
      };
      delete nextSplit.intent;
      const intent = normalizedIntentForPaneIds(
        {
          ...split,
          paneIds,
          intent: Object.fromEntries(
            Object.entries(split.intent ?? {}).filter(
              ([id, entry]) => id !== paneId && entry.anchorPaneId !== paneId,
            ),
          ),
        },
        paneIds,
      );
      if (intent) {
        nextSplit.intent = intent;
      }
      return nextSplit;
    })
    .filter((split) => split.paneIds.length >= 2);
}

export function splitFractions(split: PaneSplitInfo): number[] {
  const raw = split.paneIds.map((paneId) => split.sizes?.[paneId] ?? 0);
  const total = raw.reduce(
    (sum, value) => sum + (Number.isFinite(value) && value > 0 ? value : 0),
    0,
  );
  if (total <= 0) {
    return split.paneIds.map(() => 1 / split.paneIds.length);
  }
  const clamped = raw.map((value) => Math.max(MIN_SPLIT_FRACTION, value / total));
  const clampedTotal = clamped.reduce((sum, value) => sum + value, 0);
  return clamped.map((value) => value / clampedTotal);
}

export function resizeSplitFractions(
  split: PaneSplitInfo,
  dividerIndex: number,
  deltaFraction: number,
): PaneSplitInfo {
  const fractions = splitFractions(split);
  if (dividerIndex < 0 || dividerIndex >= fractions.length - 1) {
    return split;
  }
  const before = fractions[dividerIndex];
  const after = fractions[dividerIndex + 1];
  const pairTotal = before + after;
  const nextBefore = Math.min(
    pairTotal - MIN_SPLIT_FRACTION,
    Math.max(MIN_SPLIT_FRACTION, before + deltaFraction),
  );
  fractions[dividerIndex] = nextBefore;
  fractions[dividerIndex + 1] = pairTotal - nextBefore;
  const total = fractions.reduce((sum, value) => sum + value, 0);
  return {
    ...split,
    sizes: Object.fromEntries(
      split.paneIds.map((paneId, index) => [paneId, fractions[index] / total]),
    ),
  };
}

export function paneSplitsEqual(a: PaneSplitInfo[], b: PaneSplitInfo[]) {
  return JSON.stringify(a) === JSON.stringify(b);
}

export function canToggleTurnSidebar(
  activePaneHasTurnSidebar: boolean,
  splitRightPaneMode: boolean,
  splitTurnSidebarCount: number,
) {
  return activePaneHasTurnSidebar || (splitRightPaneMode && splitTurnSidebarCount > 0);
}
