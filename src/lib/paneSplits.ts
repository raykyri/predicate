import type {
  PaneInfo,
  PaneSplitAxis,
  PaneSplitBranchNode,
  PaneSplitInfo,
  PaneSplitIntent,
  PaneSplitIntentPosition,
  PaneSplitIntentSource,
  PaneSplitNode,
} from "../types";

const MIN_SPLIT_FRACTION = 0.12;
/** Depth ceiling for a persisted layout tree. Deeper trees are treated as
 * invalid so a corrupt file degrades to a flat split instead of recursing
 * without bound. Real layouts never approach this. */
const MAX_SPLIT_DEPTH = 16;
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
  /** Root axis for a split being created from two unsplit tabs. */
  axis?: PaneSplitAxis;
  /** Axis to lay the inserted pane out along, relative to its anchor's branch.
   * When it differs from that branch's axis the anchor leaf becomes a nested
   * branch. Omitted means "append along whatever axis the anchor already uses",
   * which is what joins and drags want. */
  nestAxis?: PaneSplitAxis;
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

/* ------------------------------------------------------------------------- *
 * Layout tree
 *
 * A split's geometry is either flat (no `root`: `paneIds` laid out along
 * `axis`) or nested (`root`: a tree whose in-order leaves are exactly
 * `paneIds`). Keeping that invariant means tab order and geometry can never
 * disagree, so every consumer of the flat pane list — sidebar bracketing, drag
 * reorder, close-selection — keeps working untouched.
 *
 * `root` is only ever present when the layout is genuinely nested; a tree whose
 * children are all panes is stored flat so nothing changes for existing splits.
 * ------------------------------------------------------------------------- */

/** Fractions of a node's children, normalized to sum to 1. Absent or
 * non-positive sizes fall to zero unless every child lacks one. */
function normalizedFractions(raw: number[]): number[] {
  const clean = raw.map((value) => (Number.isFinite(value) && value > 0 ? value : 0));
  const total = clean.reduce((sum, value) => sum + value, 0);
  if (total <= 0) {
    return clean.map(() => (clean.length > 0 ? 1 / clean.length : 1));
  }
  return clean.map((value) => value / total);
}

/** Applies the per-pane resize floor, then renormalizes. Matches the clamping
 * `splitFractions` has always applied to a flat split. */
function clampedFractions(fractions: number[]): number[] {
  const clamped = fractions.map((value) => Math.max(MIN_SPLIT_FRACTION, value));
  const total = clamped.reduce((sum, value) => sum + value, 0);
  return total > 0 ? clamped.map((value) => value / total) : fractions;
}

function nodeFractions(node: PaneSplitBranchNode): number[] {
  return clampedFractions(normalizedFractions(node.children.map((child) => child.size ?? 0)));
}

function withNodeSize(node: PaneSplitNode, size: number | undefined): PaneSplitNode {
  if (node.size === size) {
    return node;
  }
  if (size === undefined) {
    const next = { ...node };
    delete next.size;
    return next;
  }
  return { ...node, size };
}

export function paneSplitIsNested(split: PaneSplitInfo | null | undefined): boolean {
  return Boolean(split?.root);
}

/** In-order pane ids of a layout tree. Always equals the split's `paneIds`
 * for a normalized split. */
export function splitNodePaneIds(node: PaneSplitNode | null | undefined): string[] {
  if (!node) {
    return [];
  }
  if (node.kind === "pane") {
    return [node.paneId];
  }
  return node.children.flatMap((child) => splitNodePaneIds(child));
}

function splitNodePaneIdsMatch(node: PaneSplitNode, paneIds: string[]): boolean {
  const leaves = splitNodePaneIds(node);
  return (
    leaves.length === paneIds.length && leaves.every((paneId, index) => paneId === paneIds[index])
  );
}

/** The split's tree, synthesizing the equivalent flat tree when it has none, so
 * every geometry and edit path can work in one representation. */
export function paneSplitRootNode(split: PaneSplitInfo): PaneSplitBranchNode {
  const root = split.root;
  // Defensive: an un-normalized split (an optimistic drag result, say) can carry
  // a tree that still names a detached pane. Rendering that would hand stage
  // space to a pane with no surface, so fall back to the flat tree until
  // normalization prunes it.
  if (root && root.kind === "split" && splitNodePaneIdsMatch(root, split.paneIds)) {
    return root;
  }
  const fractions = splitFractions(split);
  return {
    kind: "split",
    axis: paneSplitAxis(split),
    children: split.paneIds.map((paneId, index) => ({
      kind: "pane" as const,
      paneId,
      size: fractions[index],
    })),
  };
}

/** Dotted child indices from the root, e.g. `""` for the root and `"1.0"` for
 * the first child of the root's second child. Paths (not flat divider indices)
 * key the resizers so React never reuses one across a layout change. */
function childPath(path: string, index: number): string {
  return path ? `${path}.${index}` : `${index}`;
}

export function splitNodeAtPath(
  root: PaneSplitNode,
  path: string,
): PaneSplitBranchNode | null {
  let node: PaneSplitNode = root;
  const segments = path ? path.split(".") : [];
  for (const segment of segments) {
    if (node.kind !== "split") {
      return null;
    }
    const index = Number.parseInt(segment, 10);
    const child = Number.isInteger(index) ? node.children[index] : undefined;
    if (!child) {
      return null;
    }
    node = child;
  }
  return node.kind === "split" ? node : null;
}

/** Replaces the branch at `path`, rebuilding only the spine above it. */
function withNodeAtPath(
  root: PaneSplitNode,
  path: string,
  replace: (node: PaneSplitBranchNode) => PaneSplitNode,
): PaneSplitNode {
  if (!path) {
    return root.kind === "split" ? replace(root) : root;
  }
  const [head, ...rest] = path.split(".");
  const index = Number.parseInt(head, 10);
  if (root.kind !== "split" || !Number.isInteger(index) || !root.children[index]) {
    return root;
  }
  const children = root.children.slice();
  children[index] = withNodeAtPath(children[index], rest.join("."), replace);
  return { ...root, children };
}

/** The branch holding `paneId`, with the leaf's index inside it. */
export function splitNodeParentOfPane(
  root: PaneSplitNode,
  paneId: string,
  path = "",
): { node: PaneSplitBranchNode; path: string; index: number } | null {
  if (root.kind !== "split") {
    return null;
  }
  for (const [index, child] of root.children.entries()) {
    if (child.kind === "pane") {
      if (child.paneId === paneId) {
        return { node: root, path, index };
      }
      continue;
    }
    const found = splitNodeParentOfPane(child, paneId, childPath(path, index));
    if (found) {
      return found;
    }
  }
  return null;
}

/* --- normalization ------------------------------------------------------- */

/** Shape check against untrusted persisted JSON. Structure only; membership and
 * ordering are verified afterwards against the split's `paneIds`. */
function sanitizedSplitNode(value: unknown, depth = 0): PaneSplitNode | null {
  if (depth > MAX_SPLIT_DEPTH || !value || typeof value !== "object") {
    return null;
  }
  const node = value as Partial<PaneSplitNode> & { children?: unknown };
  const size =
    typeof node.size === "number" && Number.isFinite(node.size) && node.size > 0
      ? node.size
      : undefined;
  if (node.kind === "pane") {
    const paneId = (node as Partial<{ paneId: unknown }>).paneId;
    if (typeof paneId !== "string" || !paneId) {
      return null;
    }
    return size === undefined ? { kind: "pane", paneId } : { kind: "pane", paneId, size };
  }
  if (node.kind !== "split" || !Array.isArray(node.children)) {
    return null;
  }
  const children = node.children
    .map((child) => sanitizedSplitNode(child, depth + 1))
    .filter((child): child is PaneSplitNode => Boolean(child));
  if (children.length !== node.children.length) {
    return null;
  }
  const axis: PaneSplitAxis =
    (node as Partial<{ axis: unknown }>).axis === "horizontal" ? "horizontal" : "vertical";
  return size === undefined
    ? { kind: "split", axis, children }
    : { kind: "split", axis, size, children };
}

/** Splices a same-axis child branch into its parent, scaling the grandchildren
 * by the child's own share. Keeps one layout from having two representations,
 * which matters because collapsing a pruned branch can produce one. Returns the
 * same node when there is nothing to merge, so normalization stays idempotent. */
function mergedSameAxisChildren(node: PaneSplitBranchNode): PaneSplitBranchNode {
  const nested = node.children.some(
    (child) => child.kind === "split" && child.axis === node.axis,
  );
  if (!nested) {
    return node;
  }
  const fractions = normalizedFractions(node.children.map((child) => child.size ?? 0));
  const children: PaneSplitNode[] = [];
  node.children.forEach((child, index) => {
    if (child.kind === "split" && child.axis === node.axis) {
      const inner = normalizedFractions(child.children.map((grand) => grand.size ?? 0));
      child.children.forEach((grand, grandIndex) => {
        children.push(withNodeSize(grand, fractions[index] * inner[grandIndex]));
      });
      return;
    }
    children.push(withNodeSize(child, fractions[index]));
  });
  return { ...node, children };
}

/** Drops leaves outside `keep`, removes emptied branches, collapses single-child
 * branches into their child, and merges same-axis nesting. Bottom-up, and
 * returns the original node when nothing changed. */
function prunedSplitNode(
  node: PaneSplitNode,
  keep: Set<string>,
  depth = 0,
): PaneSplitNode | null {
  if (depth > MAX_SPLIT_DEPTH) {
    return null;
  }
  if (node.kind === "pane") {
    return keep.has(node.paneId) ? node : null;
  }
  const children: PaneSplitNode[] = [];
  let changed = false;
  for (const child of node.children) {
    const pruned = prunedSplitNode(child, keep, depth + 1);
    if (pruned !== child) {
      changed = true;
    }
    if (pruned) {
      children.push(pruned);
    }
  }
  if (children.length === 0) {
    return null;
  }
  if (children.length === 1) {
    // A collapsing branch hands its share of the grandparent to the survivor.
    return withNodeSize(children[0], node.size);
  }
  return mergedSameAxisChildren(changed ? { ...node, children } : node);
}

/** Each leaf's fraction of its own parent, mirrored into the flat `sizes` map so
 * a build that predates nesting still renders a plausible flat run. */
function leafSizesFromRoot(node: PaneSplitNode, out: Record<string, number> = {}) {
  if (node.kind === "pane") {
    return out;
  }
  const fractions = normalizedFractions(node.children.map((child) => child.size ?? 0));
  node.children.forEach((child, index) => {
    if (child.kind === "pane") {
      out[child.paneId] = fractions[index];
      return;
    }
    leafSizesFromRoot(child, out);
  });
  return out;
}

/** The pruned tree for a split whose flat membership is `paneIds`, or undefined
 * when the stored tree cannot be trusted (any structural problem degrades to
 * flat rather than dropping the split).
 *
 * `nested` says whether it still needs storing: a tree whose children are all
 * panes is exactly a flat split, so `root` is omitted — but its axis and leaf
 * fractions are still the right ones to keep, because a collapse can turn
 * columns into a stack. */
function normalizedSplitRoot(
  split: PaneSplitInfo,
  paneIds: string[],
): { node: PaneSplitBranchNode; nested: boolean } | undefined {
  const sanitized = sanitizedSplitNode(split.root);
  if (!sanitized) {
    return undefined;
  }
  // Re-sanitizing canonicalizes key order after pruning and inserting, so a
  // normalized split JSON-compares equal to its own re-normalization and the
  // pane-change effect cannot ping-pong persists.
  const rebuilt = prunedSplitNode(sanitized, new Set(paneIds));
  const pruned = rebuilt ? sanitizedSplitNode(withNodeSize(rebuilt, undefined)) : null;
  if (!pruned || pruned.kind !== "split" || pruned.children.length < 2) {
    return undefined;
  }
  const leaves = splitNodePaneIds(pruned);
  if (
    leaves.length !== paneIds.length ||
    leaves.some((paneId, index) => paneId !== paneIds[index])
  ) {
    return undefined;
  }
  return {
    node: pruned,
    nested: pruned.children.some((child) => child.kind === "split"),
  };
}

/* --- geometry ------------------------------------------------------------ */

/** A pane or divider rectangle as a fraction of the terminal stage plus a pixel
 * correction, matching the `calc(F% + Ppx)` form the stage has always used.
 * Gutters are pixel-sized at every level, so they cannot be folded into the
 * fractions. */
export interface SplitRect {
  leftFraction: number;
  leftPx: number;
  widthFraction: number;
  widthPx: number;
  topFraction: number;
  topPx: number;
  heightFraction: number;
  heightPx: number;
}

/** A branch's own box plus the clamped fractions it divides among its children.
 * Resize drags and the resize mask both work in these node-local offsets, which
 * are directly comparable numbers — unlike the composed fraction/pixel pairs. */
export interface SplitBranchLayout {
  path: string;
  axis: PaneSplitAxis;
  rect: SplitRect;
  fractions: number[];
}

export interface SplitDivider {
  /** Path of the branch this divider belongs to. */
  path: string;
  /** Index of the child before the divider. */
  index: number;
  /** Axis the branch lays its children out along. */
  axis: PaneSplitAxis;
  rect: SplitRect;
}

export interface PaneSplitLayout {
  panes: Map<string, SplitRect>;
  branches: Map<string, SplitBranchLayout>;
  dividers: SplitDivider[];
}

const FULL_STAGE_RECT: SplitRect = {
  leftFraction: 0,
  leftPx: 0,
  widthFraction: 1,
  widthPx: 0,
  topFraction: 0,
  topPx: 0,
  heightFraction: 1,
  heightPx: 0,
};

/** The full-stage rectangle, for the single-pane case where there is no split.
 * A fresh object each call: callers treat rects as their own to build styles
 * from, and handing out the shared seed invites a mutation that would corrupt
 * every later layout. */
export function fullStageSplitRect(): SplitRect {
  return { ...FULL_STAGE_RECT };
}

/** Cumulative child offsets, `[0, f0, f0+f1, ..., 1]`. */
export function splitBranchOffsets(branch: SplitBranchLayout): number[] {
  const offsets = [0];
  for (const fraction of branch.fractions) {
    offsets.push(offsets[offsets.length - 1] + fraction);
  }
  return offsets;
}

/** A box inside a branch spanning `span` of its content starting at `offset`,
 * sitting after `dividerIndex` gutters. Span 0 is a divider; a positive span is
 * the region a divider has swept. The branch's own extent has to give up a
 * gutter per boundary before its children divide what is left, which is why the
 * pixel term is separate from the fraction term all the way down. */
export function splitBranchSpanRect(
  branch: SplitBranchLayout,
  offset: number,
  span: number,
  dividerIndex: number,
  gutter: number,
): SplitRect {
  const horizontal = branch.axis === "horizontal";
  const gutters = Math.max(0, branch.fractions.length - 1);
  const contentFraction = horizontal ? branch.rect.widthFraction : branch.rect.heightFraction;
  const contentPx = (horizontal ? branch.rect.widthPx : branch.rect.heightPx) - gutters * gutter;
  const originFraction = horizontal ? branch.rect.leftFraction : branch.rect.topFraction;
  const originPx = horizontal ? branch.rect.leftPx : branch.rect.topPx;
  const startFraction = originFraction + contentFraction * offset;
  const startPx = originPx + contentPx * offset + dividerIndex * gutter;
  const extentFraction = contentFraction * span;
  const extentPx = contentPx * span + gutter;
  return horizontal
    ? {
        ...branch.rect,
        leftFraction: startFraction,
        leftPx: startPx,
        widthFraction: extentFraction,
        widthPx: extentPx,
      }
    : {
        ...branch.rect,
        topFraction: startFraction,
        topPx: startPx,
        heightFraction: extentFraction,
        heightPx: extentPx,
      };
}

/** Pixels available to a branch's children after its gutters, along its axis. */
export function splitBranchContentExtent(
  branch: SplitBranchLayout,
  stage: { width: number; height: number },
  gutter: number,
): number {
  const box = splitRectPixels(branch.rect, stage);
  const extent = branch.axis === "horizontal" ? box.width : box.height;
  return extent - Math.max(0, branch.fractions.length - 1) * gutter;
}

function layoutSplitNode(
  node: PaneSplitNode,
  rect: SplitRect,
  path: string,
  gutter: number,
  out: PaneSplitLayout,
) {
  if (node.kind === "pane") {
    out.panes.set(node.paneId, rect);
    return;
  }
  const horizontal = node.axis === "horizontal";
  const branch: SplitBranchLayout = {
    path,
    axis: node.axis,
    rect,
    fractions: nodeFractions(node),
  };
  out.branches.set(path, branch);
  const offsets = splitBranchOffsets(branch);
  node.children.forEach((child, index) => {
    const slot = splitBranchSpanRect(branch, offsets[index], branch.fractions[index], index, gutter);
    // `splitBranchSpanRect` adds a trailing gutter (it is shaped for dividers and
    // swept regions); a child owns only the content before it.
    const childRect = horizontal
      ? { ...slot, widthPx: slot.widthPx - gutter }
      : { ...slot, heightPx: slot.heightPx - gutter };
    layoutSplitNode(child, childRect, childPath(path, index), gutter, out);
  });
  for (let index = 0; index < node.children.length - 1; index += 1) {
    out.dividers.push({
      path,
      index,
      axis: node.axis,
      rect: splitBranchSpanRect(branch, offsets[index + 1], 0, index, gutter),
    });
  }
}

/** Every pane rectangle, branch box and divider for a split, flat or nested. */
export function paneSplitLayout(split: PaneSplitInfo, gutter: number): PaneSplitLayout {
  const out: PaneSplitLayout = { panes: new Map(), branches: new Map(), dividers: [] };
  layoutSplitNode(paneSplitRootNode(split), FULL_STAGE_RECT, "", gutter, out);
  return out;
}

export function splitRectPixels(
  rect: SplitRect,
  stage: { width: number; height: number },
) {
  const left = rect.leftFraction * stage.width + rect.leftPx;
  const top = rect.topFraction * stage.height + rect.topPx;
  const width = rect.widthFraction * stage.width + rect.widthPx;
  const height = rect.heightFraction * stage.height + rect.heightPx;
  return { left, top, width, height, right: left + width, bottom: top + height };
}

/** One `calc(F% - Ppx)` term. The sign is written explicitly rather than
 * interpolating a negative number: `calc(50% + -4px)` is legal CSS, but this
 * renders in a WKWebView and the pre-nesting split styles emitted the
 * subtraction form, so keep producing exactly that. */
export function splitCalc(fraction: number, px: number): string {
  const percent = Number.isFinite(fraction) ? fraction * 100 : 0;
  const offset = Number.isFinite(px) ? px : 0;
  return `calc(${percent}% ${offset < 0 ? "-" : "+"} ${Math.abs(offset)}px)`;
}

/** The four absolute offsets placing a node on the stage. Every side is
 * explicit because a nested pane or divider no longer spans the stage on its
 * cross axis, and the split CSS pins the cross axis to 0/0 by default. */
export function splitRectOffsets(rect: SplitRect) {
  return {
    left: splitCalc(rect.leftFraction, rect.leftPx),
    width: splitCalc(rect.widthFraction, rect.widthPx),
    top: splitCalc(rect.topFraction, rect.topPx),
    height: splitCalc(rect.heightFraction, rect.heightPx),
  };
}

/** Which pane the point lands in, in stage-relative pixels. Replaces the
 * single-axis cursor walk the drop target used to do, which had no way to tell
 * nested panes apart. */
export function paneAtStagePoint(
  layout: PaneSplitLayout,
  stage: { width: number; height: number },
  x: number,
  y: number,
): string | null {
  for (const [paneId, rect] of layout.panes) {
    const box = splitRectPixels(rect, stage);
    if (x >= box.left && x <= box.right && y >= box.top && y <= box.bottom) {
      return paneId;
    }
  }
  return null;
}

/* --- edits --------------------------------------------------------------- */

/** Resizes the divider after child `dividerIndex` of the branch at `path`.
 * Flat splits keep using the pane-keyed `sizes` map. */
export function resizeSplitNodeFractions(
  split: PaneSplitInfo,
  path: string,
  dividerIndex: number,
  deltaFraction: number,
): PaneSplitInfo {
  if (!split.root) {
    return path === "" ? resizeSplitFractions(split, dividerIndex, deltaFraction) : split;
  }
  const target = splitNodeAtPath(split.root, path);
  if (!target || dividerIndex < 0 || dividerIndex >= target.children.length - 1) {
    return split;
  }
  const fractions = nodeFractions(target);
  const before = fractions[dividerIndex];
  const after = fractions[dividerIndex + 1];
  const pairTotal = before + after;
  const nextBefore = Math.min(
    pairTotal - MIN_SPLIT_FRACTION,
    Math.max(MIN_SPLIT_FRACTION, before + deltaFraction),
  );
  fractions[dividerIndex] = nextBefore;
  fractions[dividerIndex + 1] = pairTotal - nextBefore;
  const root = withNodeAtPath(split.root, path, (node) => ({
    ...node,
    children: node.children.map((child, index) => withNodeSize(child, fractions[index])),
  }));
  return { ...split, root };
}

/** Inserts `insertedPaneId` beside `anchorPaneId`, laid out along `axis`.
 * When the anchor's branch already runs along that axis the pane joins it as a
 * sibling; otherwise the anchor leaf becomes a two-child branch — the nesting
 * step.
 *
 * `position` must match where the new tab lands relative to the anchor in the
 * sidebar. In-order leaves have to equal the tab order or normalization rejects
 * the tree, so a dragged pane dropped on a pane's leading half inserts before
 * its anchor, not after. */
export function insertPaneIntoSplitTree(
  root: PaneSplitBranchNode,
  anchorPaneId: string,
  insertedPaneId: string,
  axis: PaneSplitAxis,
  position: "before" | "after" = "after",
): PaneSplitBranchNode | null {
  const parent = splitNodeParentOfPane(root, anchorPaneId);
  if (!parent || splitNodePaneIds(root).includes(insertedPaneId)) {
    return null;
  }
  const inserted = withNodeAtPath(root, parent.path, (node) => {
    if (node.axis === axis) {
      // Every sibling yields an equal share of the newcomer's slot, so relative
      // proportions survive. Matches what `joinedPaneSizes` does for a flat split.
      const fractions = nodeFractions(node);
      const scale = node.children.length / (node.children.length + 1);
      const share = 1 / (node.children.length + 1);
      const children = node.children.map((child, index) =>
        withNodeSize(child, fractions[index] * scale),
      );
      children.splice(position === "before" ? parent.index : parent.index + 1, 0, {
        kind: "pane",
        paneId: insertedPaneId,
        size: share,
      });
      return { ...node, children };
    }
    const anchor = node.children[parent.index];
    const children = node.children.slice();
    const pair: PaneSplitNode[] = [
      withNodeSize(anchor, 0.5),
      { kind: "pane", paneId: insertedPaneId, size: 0.5 },
    ];
    children[parent.index] = {
      kind: "split",
      axis,
      ...(anchor.size === undefined ? {} : { size: anchor.size }),
      children: position === "before" ? [pair[1], pair[0]] : pair,
    };
    return { ...node, children };
  });
  return inserted.kind === "split" ? inserted : null;
}

/** The axis the pane's own branch lays out along — the axis a same-direction
 * split would append to rather than nest inside. */
export function splitAxisForPane(
  split: PaneSplitInfo,
  paneId: string,
): PaneSplitAxis | null {
  const parent = splitNodeParentOfPane(paneSplitRootNode(split), paneId);
  return parent ? parent.node.axis : null;
}

/** Whether splitting `paneId` along `axis` leaves every resulting pane at or
 * above the resize floor. Appending shrinks the whole branch proportionally;
 * nesting only subdivides the source pane. Bounding depth by real screen area
 * beats an arbitrary nesting limit — and it is the same refusal ⌘⇧D already had
 * when a column split would not fit. */
export function canSplitPaneInTree({
  split,
  paneId,
  axis,
  stage,
  gutter,
  minWidth,
  minHeight,
}: {
  split: PaneSplitInfo;
  paneId: string;
  axis: PaneSplitAxis;
  stage: { width: number; height: number };
  gutter: number;
  minWidth: number;
  minHeight: number;
}): boolean {
  if (stage.width <= 0 || stage.height <= 0) {
    return false;
  }
  const minExtent = axis === "horizontal" ? minWidth : minHeight;
  const layout = paneSplitLayout(split, gutter);
  const parent = splitNodeParentOfPane(paneSplitRootNode(split), paneId);
  const branch = parent ? layout.branches.get(parent.path) : undefined;
  if (!parent || !branch) {
    return false;
  }
  if (branch.axis !== axis) {
    const rect = layout.panes.get(paneId);
    if (!rect) {
      return false;
    }
    const box = splitRectPixels(rect, stage);
    return ((axis === "horizontal" ? box.width : box.height) - gutter) / 2 >= minExtent;
  }
  const count = branch.fractions.length;
  // One more child means one more gutter, and every sibling yields an equal
  // share of the newcomer's slot.
  const content = splitBranchContentExtent(branch, stage, gutter) - gutter;
  if (content <= 0) {
    return false;
  }
  const scale = count / (count + 1);
  const smallest = Math.min(
    ...branch.fractions.map((fraction) => fraction * scale),
    1 / (count + 1),
  );
  return smallest * content >= minExtent;
}

/** How many children the pane's own branch has, so a spawn can estimate its
 * grid before the layout lands. A wrong estimate forces an immediate resize on
 * the fresh PTY. */
export function splitBranchChildCountForPane(
  split: PaneSplitInfo,
  paneId: string,
): number {
  const parent = splitNodeParentOfPane(paneSplitRootNode(split), paneId);
  return parent ? parent.node.children.length : 1;
}

/** Recursive minimum width: a row needs the sum of its children plus gutters, a
 * stack needs the widest of them. */
function minimumNodeWidth(
  node: PaneSplitNode,
  splitMinWidth: number,
  gutter: number,
): number {
  if (node.kind === "pane") {
    return splitMinWidth;
  }
  const children = node.children.map((child) =>
    minimumNodeWidth(child, splitMinWidth, gutter),
  );
  if (node.axis === "horizontal") {
    return (
      children.reduce((sum, width) => sum + width, 0) +
      Math.max(0, children.length - 1) * gutter
    );
  }
  return Math.max(...children);
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
    // A nested tree owns the geometry: `axis` mirrors its root (so a collapse
    // can flip the split from columns to rows) and `sizes` is derived from its
    // leaves. `normalizedSplitRoot` returns undefined for a flat or untrusted
    // tree, which lands on exactly the pre-nesting behaviour below.
    const tree = normalizedSplitRoot(split, paneIds);
    const normalizedSplit: PaneSplitInfo = {
      id: split.id,
      paneIds,
      sizes: tree
        ? leafSizesFromRoot(tree.node)
        : Object.fromEntries(
            Object.entries(split.sizes ?? {}).filter(
              ([paneId, size]) => paneIds.includes(paneId) && Number.isFinite(size) && size > 0,
            ),
          ),
    };
    if ((tree ? tree.node.axis : paneSplitAxis(split)) === "horizontal") {
      normalizedSplit.axis = "horizontal";
    }
    const intent = normalizedIntentForPaneIds(split, paneIds);
    if (intent) {
      normalizedSplit.intent = intent;
    }
    if (tree?.nested) {
      normalizedSplit.root = tree.node;
    }
    normalized.push(normalizedSplit);
  }

  return normalized;
}

export function paneSplitAxis(split: PaneSplitInfo | null | undefined): PaneSplitAxis {
  return split?.axis === "horizontal" ? "horizontal" : "vertical";
}

/** Flips every branch's axis. Rotating a nested layout has to transpose the
 * whole tree: flipping only the root would leave it sharing an axis with its
 * children, which normalization then merges away — silently flattening the
 * nesting the user built. */
function transposedSplitNode(node: PaneSplitNode): PaneSplitNode {
  if (node.kind === "pane") {
    return node;
  }
  return {
    ...node,
    axis: node.axis === "horizontal" ? "vertical" : "horizontal",
    children: node.children.map((child) => transposedSplitNode(child)),
  };
}

export function withPaneSplitAxis(split: PaneSplitInfo, axis: PaneSplitAxis): PaneSplitInfo {
  if (paneSplitAxis(split) === axis) {
    return split;
  }
  if (split.root) {
    const root = transposedSplitNode(split.root);
    const next: PaneSplitInfo = { ...split, root };
    if (axis === "horizontal") {
      next.axis = "horizontal";
    } else {
      delete next.axis;
    }
    return next;
  }
  if (axis === "horizontal") {
    return { ...split, axis: "horizontal" };
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

/** Width the terminal stage must keep so a column split stays at or above the
 * per-pane resize floor. Stacked splits (and a single pane) use `minWidth`. */
export function reservedTerminalStageWidth({
  axis,
  paneCount,
  minWidth,
  splitMinWidth,
  gutter,
  root,
}: {
  axis: PaneSplitAxis;
  paneCount: number;
  minWidth: number;
  splitMinWidth: number;
  gutter: number;
  /** The nested layout, when there is one. Rows add their children's widths;
   * stacks only need their widest child. */
  root?: PaneSplitNode | null;
}): number {
  if (root) {
    return Math.max(minWidth, minimumNodeWidth(root, splitMinWidth, gutter));
  }
  if (axis === "horizontal" && paneCount >= 2) {
    return paneCount * splitMinWidth + Math.max(0, paneCount - 1) * gutter;
  }
  return minWidth;
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
  // An existing split keeps its axis. `options.axis` only applies when this
  // call is creating a new split (neither pane was already grouped).
  if ((axisSource ? paneSplitAxis(axisSource) : options.axis) === "horizontal") {
    joinedSplit.axis = "horizontal";
  }
  const intent = joinedPaneIntent(existing, paneIds, paneId, belowPaneId, options);
  if (intent) {
    joinedSplit.intent = intent;
  }

  // Carry the anchor split's nesting through the join, extending it with the
  // inserted pane. Merging two separate splits has no single tree to extend, so
  // it falls back to flat. `axis` is kept in step with the root here because
  // this result is rendered optimistically before normalization runs.
  const nestBase = existing.length === 1 ? existing[0] : null;
  if (nestBase && (nestBase.root || options.nestAxis)) {
    const inserted = options.insertedPaneId;
    // `paneId` is the leading pane of the pair, so when it is the newcomer the
    // new tab sits *before* the anchor — a drag onto a pane's leading half.
    const insertedLeads = inserted === paneId;
    const anchorPaneId = insertedLeads ? belowPaneId : paneId;
    const root =
      inserted && !nestBase.paneIds.includes(inserted) && nestBase.paneIds.includes(anchorPaneId)
        ? insertPaneIntoSplitTree(
            paneSplitRootNode(nestBase),
            anchorPaneId,
            inserted,
            options.nestAxis ?? splitAxisForPane(nestBase, anchorPaneId) ?? paneSplitAxis(nestBase),
            insertedLeads ? "before" : "after",
          )
        : (nestBase.root ?? null);
    if (root && root.kind === "split" && root.children.some((child) => child.kind === "split")) {
      joinedSplit.root = root;
      if (root.axis === "horizontal") {
        joinedSplit.axis = "horizontal";
      } else {
        delete joinedSplit.axis;
      }
    }
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
      // Prune the tree here too. A drag that reorders inside its own split
      // detaches and rejoins in one gesture, and a tree still naming the
      // detached pane would be rejected on the rejoin — silently flattening a
      // nested layout.
      if (split.root) {
        const pruned = prunedSplitNode(split.root, new Set(paneIds));
        if (pruned && pruned.kind === "split") {
          nextSplit.root = pruned;
        } else {
          delete nextSplit.root;
        }
      }
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
