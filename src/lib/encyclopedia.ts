// Client-side encyclopedia helpers: types mirroring the backend's
// src-tauri/src/encyclopedia.rs, link routing for rendered pages, and the
// auto-update scheduling decisions. Pure and DOM-free so the whole surface's
// behavior pins down in tests/encyclopedia.test.ts.

export interface EncyclopediaPageInfo {
  fileName: string;
  title: string;
  updatedAt: number;
}

export interface EncyclopediaStatus {
  workspaceId: string;
  enabled: boolean;
  autoUpdate: boolean;
  updating: boolean;
  activeAgentId?: string;
  lastGeneratedAt: number;
  pendingSourceCount: number;
  pages: EncyclopediaPageInfo[];
  lastError?: string;
}

export interface EncyclopediaPageContent {
  fileName: string;
  title: string;
  markdown: string;
  updatedAt: number;
}

/** Quiet period after the last new source before an auto-update launches, so
 * a burst of completing runs coalesces into one generation. */
export const ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS = 30_000;

/** Floor between auto-launch attempts. A launch that failed (adapter missing,
 * workspace busy) must not retry in a tight loop off every status refresh. */
export const ENCYCLOPEDIA_AUTO_UPDATE_MIN_INTERVAL_MS = 5 * 60_000;

/** Mirror of the backend's page-name rule (encyclopedia.rs
 * is_valid_encyclopedia_file_name): ASCII word-ish names, `.md`, no
 * separators, no leading dot — both sides pin the shared cases in tests so
 * drift is caught. */
export function isEncyclopediaPageFileName(name: string): boolean {
  if (!name.endsWith(".md") || name.length <= 3 || name.length > 128) {
    return false;
  }
  const stem = name.slice(0, -3);
  return !stem.startsWith(".") && /^[A-Za-z0-9._ -]+$/.test(stem);
}

export type EncyclopediaLink =
  | { kind: "citation"; treeId: string; nodeId: string }
  | { kind: "page"; fileName: string }
  | { kind: "external"; url: string };

/** The inert base host safeHref resolves relative links against (links.ts).
 * A relative link inside a rendered page — `topic.md`, `/research/t/n` —
 * arrives here as an absolute URL on this host; anything on a real host is an
 * ordinary external link. */
const INTERNAL_LINK_HOST = "qmux.invalid";

/** Routes a clicked link inside a rendered encyclopedia page: a chat
 * citation (`/research/<treeId>/<nodeId>`), a sibling page (`<name>.md`), or
 * an external URL to hand to the app's normal link opening. Unrecognized
 * internal shapes fall out as external so they visibly fail safe rather than
 * silently doing nothing surprising. */
export function parseEncyclopediaHref(href: string): EncyclopediaLink {
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return { kind: "external", url: href };
  }
  if (url.hostname !== INTERNAL_LINK_HOST) {
    return { kind: "external", url: href };
  }
  const segments = url.pathname
    .split("/")
    .filter((segment) => segment.length > 0)
    .map((segment) => {
      try {
        return decodeURIComponent(segment);
      } catch {
        return segment;
      }
    });
  if (
    segments.length === 3 &&
    segments[0] === "research" &&
    segments[1].length > 0 &&
    segments[2].length > 0
  ) {
    return { kind: "citation", treeId: segments[1], nodeId: segments[2] };
  }
  const last = segments[segments.length - 1];
  if (segments.length === 1 && last !== undefined && isEncyclopediaPageFileName(last)) {
    return { kind: "page", fileName: last };
  }
  return { kind: "external", url: href };
}

/** Whether the surface should be arming an auto-update timer at all: the
 * scoped workspace has the feature on, material is pending, and no run is
 * already in flight. */
export function shouldScheduleEncyclopediaAutoUpdate(
  status: EncyclopediaStatus | null,
  scope: string | null,
): boolean {
  return Boolean(
    status &&
      scope &&
      status.workspaceId === scope &&
      status.enabled &&
      status.autoUpdate &&
      !status.updating &&
      status.pendingSourceCount > 0,
  );
}

/** Delay before the next auto-launch attempt: the debounce, stretched so
 * attempts stay at least the minimum interval apart. */
export function nextEncyclopediaAutoUpdateDelay(
  now: number,
  lastAttemptAt: number | null,
): number {
  if (lastAttemptAt === null) {
    return ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS;
  }
  return Math.max(
    ENCYCLOPEDIA_AUTO_UPDATE_DEBOUNCE_MS,
    lastAttemptAt + ENCYCLOPEDIA_AUTO_UPDATE_MIN_INTERVAL_MS - now,
  );
}

/** True when an agent event for this id can settle the tracked update run,
 * so the surface refetches status (which reaps the run backend-side). */
export function isEncyclopediaRunAgentEvent(
  status: EncyclopediaStatus | null,
  agentId: string | null | undefined,
): boolean {
  return Boolean(status?.updating && agentId && status.activeAgentId === agentId);
}
