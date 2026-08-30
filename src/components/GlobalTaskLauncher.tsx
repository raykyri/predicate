import { getCurrentWindow } from "@tauri-apps/api/window";
import { ChevronDown } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { composerPolicyFor, getAgentUiAdapter, hydrateAdapterPolicies } from "../adapters";
import {
  dismissGlobalTaskLauncher,
  forkAgent,
  getRuntimeConfig,
  listAgentTurnQueue,
  listAgents,
  listGroups,
  listPanes,
  listShellAgentJobs,
  listTurns,
  listenToEvents,
  queueDeliveryAgentTurn,
  queueWaitAgentTurn,
  submitAgentTurn,
  submitPaneInput,
  unpauseAgent,
} from "../lib/api";
import {
  agentCanFork,
  agentStatusLabel,
  defaultPaneTitle,
  latestAssistantTurnText,
  latestTurnTimestamp,
  latestUserTurnText,
} from "../lib/appHelpers";
import { formatRelativeTime } from "../lib/transcriptSessions";
import { collapseImageMarkers } from "../lib/imageMarkers";
import { sanitizeTerminalTitle } from "../lib/terminalTitle";
import {
  stripTaggedInstructionBlocks,
  stripTaggedUserInstructionBlocks,
} from "../lib/taggedInstructions";
import {
  FORK_REQUIREMENT_TITLE,
  QUEUE_DELIVERY_OPTIONS,
  agentTabStatusDotClass,
  agentTabStatusPill,
  composerSlashCommandSubmitLabels,
  deriveComposerGating,
  planComposerSubmission,
  queueWaitsOnOtherAgent,
  waitTargetStatusDotClass,
  waitTargetStatusLabel,
} from "../lib/composerActions";
import { parseComposerSlashCommand } from "../lib/composerSlashCommands";
import { bodyFontStackFor, loadSettings } from "../lib/settings";
import {
  clearSessionDraft,
  loadSessionDraftJson,
  readSessionDraftJson,
  saveSessionDraftJson,
  SESSION_DRAFT_KEYS,
} from "../lib/sessionDrafts";
import {
  ComposerSubmitShortcutGlyph,
  isComposerSubmitShortcut,
} from "./ComposerSubmitShortcut";
import type {
  AgentInfo,
  GroupInfo,
  PaneInfo,
  QueuedTurn,
  RuntimeConfig,
  ShellAgentJobInfo,
  SubmitAgentTurnMode,
  WaitTarget,
} from "../types";

interface LauncherTarget {
  agent: AgentInfo;
  pane: PaneInfo;
  group: GroupInfo | undefined;
  shellJob: ShellAgentJobInfo | undefined;
  queue: QueuedTurn[];
  title: string;
}

/** Mirrors the sidebar's displayPaneTitle rule: the live terminal (OSC) title
 * only stands in while the pane still carries its default title, so a renamed
 * or auto-titled tab keeps the name the user knows it by. (The sidebar also
 * tracks renames made this session in memory; the stored-title comparison is
 * the durable half of that rule and matches the sidebar after a restart.) */
function resolveTargetTitle(
  pane: PaneInfo,
  agent: AgentInfo,
  config: RuntimeConfig | null,
): string {
  const oscTitle = pane.lastOscTitle
    ? sanitizeTerminalTitle(pane.lastOscTitle, agent.adapter)
    : null;
  const fallback = defaultPaneTitle(pane, agent, config);
  return oscTitle && fallback !== null && pane.title === fallback ? oscTitle : pane.title;
}

/** Hover summary for a target row: the detail the old dropdown label spelled
 * out (tab kind, status, queue depth, shell job) now lives in the tooltip. */
function targetDetails(target: LauncherTarget): string {
  const kind = target.pane.kind === "shell" ? "Shell tab" : "Agent tab";
  const queued = target.queue.length > 0 ? `${target.queue.length} queued` : null;
  const shell = target.shellJob ? `shell ${target.shellJob.state}` : null;
  return [target.title, kind, agentStatusLabel(target.agent.status), queued, shell]
    .filter(Boolean)
    .join(" · ");
}

function targetOptionDomId(agentId: string): string {
  return `global-task-launcher-option-${agentId}`;
}

/** The tail of the last exchange with a target: what the user last sent, the
 * agent's latest reply, and when the conversation last moved, so a summon
 * shows whether it continues an existing conversation. */
interface AgentExchange {
  userText: string | null;
  agentText: string | null;
  at: number | null;
}

/** One display line for the context strip: injected instruction blocks and
 * image markers removed, whitespace collapsed to a single line. The You row
 * keeps the tail rather than the head — the end of the last message is what
 * says where the conversation left off. */
// The visible preview and the DOM tooltip title are both bounded before they
// are stored. A target's last reply is agent-authored and can be many
// megabytes; without a bound the always-on-top launcher runs several
// full-string transformations and inserts the whole thing twice (span + title)
// on every summon. Slicing the raw source to a small working window first keeps
// even the clean/normalize passes cheap.
const EXCHANGE_RAW_WINDOW_CHARS = 4_000;
const EXCHANGE_PREVIEW_CHARS = 110;
const EXCHANGE_TITLE_CHARS = 500;

function exchangeLine(
  raw: string | null,
  clean: (text: string) => string,
  tailBiased: boolean,
): { text: string; title: string } | null {
  if (!raw) return null;
  const windowed =
    raw.length > EXCHANGE_RAW_WINDOW_CHARS
      ? tailBiased
        ? raw.slice(-EXCHANGE_RAW_WINDOW_CHARS)
        : raw.slice(0, EXCHANGE_RAW_WINDOW_CHARS)
      : raw;
  const cleaned = collapseImageMarkers(clean(windowed)).trim();
  if (!cleaned) return null;
  const collapsed = cleaned.replace(/\s+/g, " ");
  const text =
    collapsed.length > EXCHANGE_PREVIEW_CHARS
      ? tailBiased
        ? `…${collapsed.slice(-EXCHANGE_PREVIEW_CHARS)}`
        : `${collapsed.slice(0, EXCHANGE_PREVIEW_CHARS)}…`
      : collapsed;
  const title =
    cleaned.length > EXCHANGE_TITLE_CHARS ? `${cleaned.slice(0, EXCHANGE_TITLE_CHARS)}…` : cleaned;
  return { text, title };
}

/** Matches the main window's tab-cycling chords (⌃Tab / ⌃⇧Tab and ⌘⇧[ / ⌘⇧])
 * so retargeting the launcher rides the same muscle memory without leaving the
 * draft. Brackets match the produced character — with the same "{"/"}"
 * normalization as the main window's shortcut parser — not the physical key,
 * so non-US layouts agree with the main window about which chord this is. */
function cycleChordDirection(event: KeyboardEvent): -1 | 1 | null {
  if (!event.metaKey && event.ctrlKey && !event.altKey && event.key === "Tab") {
    return event.shiftKey ? -1 : 1;
  }
  if (event.metaKey && !event.ctrlKey && !event.altKey && event.shiftKey) {
    if (event.key === "[" || event.key === "{") return -1;
    if (event.key === "]" || event.key === "}") return 1;
  }
  return null;
}

/* The launcher window outlives individual launches, but the app restarts;
 * remembering the last explicit target lets a fresh window open on the tab the
 * user most recently dispatched to instead of the first row. */
const LAST_TARGET_STORAGE_KEY = "qmux.globalTaskLauncher.lastTarget.v1";

function loadLastTargetAgentId(): string {
  try {
    return localStorage.getItem(LAST_TARGET_STORAGE_KEY) ?? "";
  } catch {
    return "";
  }
}

function saveLastTargetAgentId(agentId: string): void {
  try {
    localStorage.setItem(LAST_TARGET_STORAGE_KEY, agentId);
  } catch {
    // Storage unavailable; target memory just won't survive restarts.
  }
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

export default function GlobalTaskLauncher() {
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const filterInputRef = useRef<HTMLInputElement | null>(null);
  const refreshTimerRef = useRef<number | null>(null);
  // Adapter labels for default-title resolution; fetched once (retried until
  // it succeeds) since the adapter set is fixed for the app's lifetime.
  const runtimeConfigRef = useRef<RuntimeConfig | null>(null);
  // The launcher window is created hidden and kept alive for the whole app
  // lifetime, so without this gate its webview would refetch panes, agents,
  // groups, shell jobs, and every agent's turn queue on every backend event —
  // ~12×/second under a busy agent — while never on screen. Only the visible
  // window needs live data; focus/blur drive this flag.
  const visibleRef = useRef(false);
  const [targets, setTargets] = useState<LauncherTarget[]>([]);
  const [selectedAgentId, setSelectedAgentId] = useState<string>(loadLastTargetAgentId);
  const [filter, setFilter] = useState("");
  const [initialDraftValue] = useState(
    () => readSessionDraftJson<{ text: string }>(SESSION_DRAFT_KEYS.globalTaskLauncher)?.text ?? "",
  );
  const [value, setValue] = useState(initialDraftValue);
  const [sessionDraftReady, setSessionDraftReady] = useState(Boolean(initialDraftValue));
  const valueTouchedRef = useRef(false);
  const [queueMenuOpen, setQueueMenuOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // Mirror the composer's submit shortcut ("Require ⌘↵ to send"). Settings live
  // in localStorage, which this window shares with the main one; re-read on
  // each focus so a change made in Settings applies to the next launch.
  const [requireCmdEnterToSend, setRequireCmdEnterToSend] = useState(
    () => loadSettings().requireCmdEnterToSend,
  );
  // Last-exchange context per target, cached for the lifetime of one summon
  // (the cache clears on each window show). Transcripts can be large, so
  // retargeting hits the cache instead of refetching; a reply that lands while
  // the window is open shows on the next summon.
  const exchangeCacheRef = useRef(new Map<string, AgentExchange>());
  const exchangeRequestRef = useRef(0);
  const [exchange, setExchange] = useState<AgentExchange | null>(null);
  // Bumped on each window show so the effect below refetches through the
  // just-cleared cache even when the selected target didn't change.
  const [exchangeEpoch, setExchangeEpoch] = useState(0);

  useEffect(() => {
    if (initialDraftValue) {
      return;
    }
    let disposed = false;
    void loadSessionDraftJson<{ text: string }>(SESSION_DRAFT_KEYS.globalTaskLauncher)
      .then((restored) => {
        if (!disposed && restored?.text && !valueTouchedRef.current) {
          setValue(restored.text);
        }
      })
      .catch(() => undefined)
      .finally(() => {
        if (!disposed) {
          setSessionDraftReady(true);
        }
      });
    return () => {
      disposed = true;
    };
  }, [initialDraftValue]);

  useEffect(() => {
    if (!sessionDraftReady) {
      return;
    }
    if (value) {
      saveSessionDraftJson(SESSION_DRAFT_KEYS.globalTaskLauncher, { text: value });
    } else {
      clearSessionDraft(SESSION_DRAFT_KEYS.globalTaskLauncher);
    }
  }, [sessionDraftReady, value]);

  useLayoutEffect(() => {
    const settings = loadSettings();
    const root = document.documentElement;
    root.dataset.colorTheme = settings.colorTheme;
    root.style.setProperty("--font-ui", bodyFontStackFor(settings.bodyFontId));
    return () => {
      delete root.dataset.colorTheme;
      root.style.removeProperty("--font-ui");
    };
  }, []);

  const refresh = useCallback(async () => {
    try {
      if (runtimeConfigRef.current === null) {
        runtimeConfigRef.current = await getRuntimeConfig().catch(
          (): RuntimeConfig | null => null,
        );
        // The launcher runs in its own webview: hydrate the composer tables
        // here too so its buttons match the backend from the first render.
        if (runtimeConfigRef.current) {
          hydrateAdapterPolicies(runtimeConfigRef.current.adapters);
        }
      }
      const [panes, agents, groups, shellJobs] = await Promise.all([
        listPanes(),
        listAgents(),
        listGroups(),
        listShellAgentJobs().catch((): ShellAgentJobInfo[] => []),
      ]);
      const paneById = new Map(panes.map((pane) => [pane.id, pane]));
      const groupById = new Map(groups.map((group) => [group.id, group]));
      const shellJobByAgent = new Map(shellJobs.map((job) => [job.agentId, job]));
      const live = agents.filter(
        (agent) =>
          agent.paneId &&
          agent.status !== "failed" &&
          paneById.has(agent.paneId) &&
          paneById.get(agent.paneId)?.status !== "exited" &&
          paneById.get(agent.paneId)?.status !== "killed",
      );
      const queues = await Promise.all(
        live.map((agent) => listAgentTurnQueue(agent.id).catch((): QueuedTurn[] => [])),
      );
      const next = live
        .map((agent, index): LauncherTarget => {
          const pane = paneById.get(agent.paneId as string) as PaneInfo;
          return {
            agent,
            pane,
            group: groupById.get(pane.groupId),
            shellJob: shellJobByAgent.get(agent.id),
            queue: queues[index],
            title: resolveTargetTitle(pane, agent, runtimeConfigRef.current),
          };
        })
        .sort((left, right) => {
          const paneOrder = panes.indexOf(left.pane) - panes.indexOf(right.pane);
          return paneOrder || left.agent.createdAt - right.agent.createdAt;
        });
      setTargets(next);
      setSelectedAgentId((current) =>
        next.some((target) => target.agent.id === current) ? current : (next[0]?.agent.id ?? ""),
      );
      setError(null);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    const launcherWindow = getCurrentWindow();
    let disposed = false;
    let unlistenFocus: (() => void) | undefined;
    let unlistenEvents: (() => void) | undefined;

    void launcherWindow.onFocusChanged(({ payload: focused }) => {
      if (focused) {
        visibleRef.current = true;
        setRequireCmdEnterToSend(loadSettings().requireCmdEnterToSend);
        // A stale filter from the previous launch would silently hide tabs,
        // and cached exchange context from the previous summon may predate
        // replies that landed in between.
        setFilter("");
        exchangeCacheRef.current.clear();
        setExchangeEpoch((epoch) => epoch + 1);
        // Focus immediately: the textarea stays enabled through the refresh so
        // the first keystrokes after the hotkey land in the draft.
        requestAnimationFrame(() => textareaRef.current?.focus());
        void refresh();
      } else {
        visibleRef.current = false;
        if (refreshTimerRef.current !== null) {
          window.clearTimeout(refreshTimerRef.current);
          refreshTimerRef.current = null;
        }
        setQueueMenuOpen(false);
        void launcherWindow.hide();
      }
    }).then((unlisten) => {
      if (disposed) unlisten();
      else unlistenFocus = unlisten;
    });
    void listenToEvents(() => {
      // Skip the refetch while hidden; the focus handler refreshes on show.
      if (!visibleRef.current) return;
      if (refreshTimerRef.current !== null) window.clearTimeout(refreshTimerRef.current);
      refreshTimerRef.current = window.setTimeout(() => void refresh(), 80);
    }).then((unlisten) => {
      if (disposed) unlisten();
      else unlistenEvents = unlisten;
    });
    // One-time pre-warm so the first show renders instantly; ongoing refreshes
    // are gated on visibility above.
    void refresh();
    return () => {
      disposed = true;
      unlistenFocus?.();
      unlistenEvents?.();
      if (refreshTimerRef.current !== null) window.clearTimeout(refreshTimerRef.current);
    };
  }, [refresh]);

  // The filter only applies while its input is rendered (2+ targets); gating
  // the needle too keeps the list truthful for the render in between a target
  // dying and the cleanup effect below clearing the leftover text.
  const filterActive = targets.length > 1;
  const visibleTargets = useMemo(() => {
    // Same substring matching as the ⌘K palette, over the fields the column
    // shows: tab title and group name.
    const needle = filterActive ? filter.trim().toLowerCase() : "";
    if (needle.length === 0) return targets;
    return targets.filter(
      (target) =>
        target.title.toLowerCase().includes(needle) ||
        (target.group?.name ?? "").toLowerCase().includes(needle),
    );
  }, [targets, filter, filterActive]);

  // Clear leftover filter text when the input unmounts so a later remount
  // doesn't resume a stale query.
  useEffect(() => {
    if (!filterActive) setFilter((current) => (current ? "" : current));
  }, [filterActive]);

  // A filter that hides the selected target snaps selection to the first match
  // so the highlighted row is always the one a submit would hit — and closes
  // the queue menu, which would otherwise silently retarget mid-flight.
  // Automatic moves bypass selectTarget: only explicit picks and submissions
  // update the remembered target.
  useEffect(() => {
    if (visibleTargets.length === 0) return;
    if (!visibleTargets.some((target) => target.agent.id === selectedAgentId)) {
      setSelectedAgentId(visibleTargets[0].agent.id);
      setQueueMenuOpen(false);
    }
  }, [visibleTargets, selectedAgentId]);

  // Deriving the selection from the *visible* list keeps every downstream
  // consumer honest: when the filter matches nothing, there is no selected
  // target, so the compose column disables rather than dispatching to a tab
  // the list says doesn't exist.
  const selected =
    visibleTargets.find((target) => target.agent.id === selectedAgentId) ?? null;

  // Fetch the selected target's last exchange (cache-first). The request
  // counter advances on every selection change — including cache hits — so an
  // in-flight fetch for a previous target can never overwrite a newer one.
  const exchangeAgentId = selected?.agent.id ?? "";
  const exchangeAdapter = selected?.agent.adapter ?? "";
  useEffect(() => {
    const request = ++exchangeRequestRef.current;
    if (!exchangeAgentId) {
      setExchange(null);
      return;
    }
    const cached = exchangeCacheRef.current.get(exchangeAgentId);
    if (cached) {
      setExchange(cached);
      return;
    }
    setExchange(null);
    void listTurns(exchangeAgentId)
      .then((turns) => {
        if (exchangeRequestRef.current !== request) return;
        const normalized =
          getAgentUiAdapter(exchangeAdapter).normalizeTurns?.(turns) ?? turns;
        const next: AgentExchange = {
          userText: latestUserTurnText(normalized),
          agentText: latestAssistantTurnText(normalized),
          at: latestTurnTimestamp(normalized),
        };
        exchangeCacheRef.current.set(exchangeAgentId, next);
        setExchange(next);
      })
      .catch(() => {
        // Context is a nicety: a failed transcript read leaves the strip
        // queue-only instead of surfacing an error.
        if (exchangeRequestRef.current === request) setExchange(null);
      });
  }, [exchangeAgentId, exchangeAdapter, exchangeEpoch]);
  const policy = selected
    ? composerPolicyFor(selected.agent.adapter)
    : null;
  const {
    canSend,
    canSteer,
    canAppendQueue,
    submitShortcutWouldTargetSend,
    submitShortcutWouldTargetQueue,
  } = deriveComposerGating(
    policy,
    selected?.agent.status ?? null,
    selected?.queue.length ?? 0,
    submitting,
  );
  const canFork = agentCanFork(selected?.agent);
  const hasValue = value.trim().length > 0;
  const parsedSlashCommand = useMemo(() => parseComposerSlashCommand(value), [value]);
  const slashCommand =
    parsedSlashCommand.kind !== "none" ? parsedSlashCommand.command : null;
  const slashSubmitLabels = slashCommand
    ? composerSlashCommandSubmitLabels(slashCommand)
    : null;
  const slashCanQueue = parsedSlashCommand.kind === "ready" && canAppendQueue;
  const permissionActions =
    selected?.agent.status === "awaitingPermission" ? (policy?.permissionActions ?? []) : [];

  // Targets bucketed into runs of consecutive rows sharing a group, so the
  // rendered order always mirrors visibleTargets — the order every keyboard
  // path steps through. (Pane order can interleave groups; a group that
  // appears twice just gets its header twice.)
  const targetGroups = useMemo(() => {
    const buckets: { key: string; name: string | null; targets: LauncherTarget[] }[] = [];
    for (const target of visibleTargets) {
      const key = target.group?.id ?? "";
      const last = buckets[buckets.length - 1];
      if (last && last.key === key) {
        last.targets.push(target);
      } else {
        buckets.push({
          key,
          name: target.group?.name?.trim() || null,
          targets: [target],
        });
      }
    }
    return buckets;
  }, [visibleTargets]);

  const waitTargets = useMemo<WaitTarget[]>(
    () =>
      targets.flatMap((target) => {
        if (!selected || target.agent.id === selected.agent.id) return [];
        const active =
          ["starting", "running", "awaitingInput", "awaitingPermission"].includes(
            target.agent.status,
          ) || target.queue.length > 0;
        if (!active) return [];
        return [
          {
            agentId: target.agent.id,
            paneId: target.pane.id,
            label: target.title,
            status: target.agent.status,
            queueCount: target.queue.length,
            queueBlocked: Boolean(target.queue[0]?.waitFor),
          },
        ];
      }),
    [selected, targets],
  );

  // Every path here is an explicit pick (click, list arrows, filter arrows,
  // cycle chord), so it doubles as the write point for the remembered target.
  // "keep" leaves focus where it is — the cycle chord retargets mid-draft —
  // but still scrolls the new selection into view.
  function selectTarget(agentId: string, focus: "textarea" | "option" | "keep") {
    setSelectedAgentId(agentId);
    saveLastTargetAgentId(agentId);
    setQueueMenuOpen(false);
    requestAnimationFrame(() => {
      if (focus === "textarea") textareaRef.current?.focus();
      else if (focus === "option") document.getElementById(targetOptionDomId(agentId))?.focus();
      else {
        document
          .getElementById(targetOptionDomId(agentId))
          ?.scrollIntoView({ block: "nearest" });
      }
    });
  }

  /** One mover for all keyboard paths (list arrows, filter arrows, cycle
   * chord) so they can't drift: steps clamp or wrap per caller, Home/End are
   * absolute. */
  function moveSelection(
    step: -1 | 1 | "home" | "end",
    wrap: boolean,
    focus: "option" | "keep",
  ) {
    if (visibleTargets.length === 0) return;
    const index = visibleTargets.findIndex((target) => target.agent.id === selectedAgentId);
    const last = visibleTargets.length - 1;
    const next =
      step === "home"
        ? 0
        : step === "end"
          ? last
          : wrap
            ? (index + step + visibleTargets.length) % visibleTargets.length
            : Math.min(Math.max(index + step, 0), last);
    const target = visibleTargets[next];
    if (target && target.agent.id !== selectedAgentId) {
      selectTarget(target.agent.id, focus);
    }
  }

  // Escape and the cycle chords live on window, not on <main>: a live refresh
  // can unmount the focused option row, dropping DOM focus to <body>, where
  // events never reach React handlers inside #root. Escape pops one layer at
  // a time — queue menu, then a focused filter's text, then the window —
  // mirroring the composer, where Escape closes menus instead of the surface.
  const windowKeydownRef = useRef<(event: KeyboardEvent) => void>(() => {});
  windowKeydownRef.current = (event: KeyboardEvent) => {
    if (event.key === "Escape") {
      event.preventDefault();
      if (queueMenuOpen) setQueueMenuOpen(false);
      else if (filter && document.activeElement === filterInputRef.current) setFilter("");
      else void dismissGlobalTaskLauncher();
      return;
    }
    const direction = cycleChordDirection(event);
    if (direction !== null) {
      event.preventDefault();
      moveSelection(direction, true, "keep");
    }
  };
  useEffect(() => {
    const listener = (event: KeyboardEvent) => windowKeydownRef.current(event);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);

  async function finishSubmission(operation: () => Promise<unknown>, allowEmpty = false) {
    if (!selected || submitting || (!allowEmpty && !hasValue)) return;
    setSubmitting(true);
    setError(null);
    try {
      await operation();
      // A successful dispatch is the strongest signal for the remembered
      // target — it also covers selections made by the filter's auto-snap,
      // which selectTarget never sees.
      saveLastTargetAgentId(selected.agent.id);
      setValue("");
      clearSessionDraft(SESSION_DRAFT_KEYS.globalTaskLauncher);
      setQueueMenuOpen(false);
      // Explicit dismissal: hand focus back to the app the launcher was
      // summoned from rather than leaving qmux activated.
      await dismissGlobalTaskLauncher();
    } catch (cause) {
      setError(errorMessage(cause));
      requestAnimationFrame(() => textareaRef.current?.focus());
    } finally {
      setSubmitting(false);
    }
  }

  function submit(mode: SubmitAgentTurnMode) {
    if (!selected) return;
    const plan = planComposerSubmission(parsedSlashCommand, canFork);
    if (plan.kind === "reject") {
      setError(plan.message);
      return;
    }
    if (plan.kind === "fork" && mode !== "queue") {
      void finishSubmission(() =>
        forkAgent(selected.pane.id, {
          useWorktree: plan.useWorktree,
          prompt: plan.prompt,
        }),
      );
      return;
    }
    void finishSubmission(() => submitAgentTurn(selected.agent.id, value.trim(), mode));
  }

  function submitDefault() {
    if (parsedSlashCommand.kind !== "none") {
      submit(slashCanQueue ? "queue" : "send");
    } else if (submitShortcutWouldTargetSend) submit("send");
    else if (submitShortcutWouldTargetQueue) submit("queue");
  }

  const youLine = exchangeLine(
    exchange?.userText ?? null,
    stripTaggedUserInstructionBlocks,
    true,
  );
  const agentLine = exchangeLine(
    exchange?.agentText ?? null,
    stripTaggedInstructionBlocks,
    false,
  );
  const queuedTurns = selected?.queue ?? [];
  const hasExchange = Boolean(youLine || agentLine);
  const contextHeader = hasExchange
    ? [
        "Last exchange",
        exchange?.at != null ? formatRelativeTime(exchange.at) : null,
        queuedTurns.length > 0 ? `${queuedTurns.length} queued` : null,
      ]
        .filter(Boolean)
        .join(" · ")
    : `Queued (${queuedTurns.length})`;

  return (
    <main className="global-task-launcher">
      <aside
        className="global-task-launcher-targets"
        aria-labelledby="global-task-launcher-targets-label"
      >
        <div
          className="global-task-launcher-targets-header"
          id="global-task-launcher-targets-label"
        >
          Send task to
        </div>
        {filterActive ? (
          <input
            ref={filterInputRef}
            className="form-field global-task-launcher-filter"
            type="text"
            placeholder="Filter tabs…"
            aria-label="Filter agent tabs"
            aria-controls="global-task-launcher-target-list"
            aria-activedescendant={
              selected ? targetOptionDomId(selected.agent.id) : undefined
            }
            value={filter}
            onChange={(event) => setFilter(event.currentTarget.value)}
            onKeyDown={(event) => {
              // Palette-style keys: arrows pick without leaving the field,
              // Enter hands off to the draft. (Escape layering lives in the
              // window keydown handler.)
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                moveSelection(event.key === "ArrowDown" ? 1 : -1, true, "keep");
                return;
              }
              if (event.key === "Enter") {
                event.preventDefault();
                textareaRef.current?.focus();
              }
            }}
          />
        ) : null}
        {targets.length === 0 ? (
          <div className="global-task-launcher-target-empty">
            {loading ? "Loading agent tabs…" : "No live agent tabs"}
          </div>
        ) : visibleTargets.length === 0 ? (
          <div className="global-task-launcher-target-empty">No matching tabs</div>
        ) : (
          <div
            className="global-task-launcher-target-list"
            id="global-task-launcher-target-list"
            role="listbox"
            aria-labelledby="global-task-launcher-targets-label"
            onKeyDown={(event) => {
              if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                moveSelection(event.key === "ArrowDown" ? 1 : -1, false, "option");
              } else if (event.key === "Home" || event.key === "End") {
                event.preventDefault();
                moveSelection(event.key === "Home" ? "home" : "end", false, "option");
              }
            }}
          >
            {targetGroups.map((bucket, bucketIndex) => {
              const labelId = bucket.name
                ? `global-task-launcher-group-${bucketIndex}`
                : undefined;
              return (
                <div
                  key={`${bucket.key || "ungrouped"}:${bucketIndex}`}
                  className="global-task-launcher-target-group"
                  role="group"
                  aria-labelledby={labelId}
                >
                  {bucket.name ? (
                    <div
                      className="global-task-launcher-group-label"
                      id={labelId}
                      title={bucket.name}
                    >
                      {bucket.name}
                    </div>
                  ) : null}
                  {bucket.targets.map((target) => {
                    const isSelected = target.agent.id === selectedAgentId;
                    const waits = queueWaitsOnOtherAgent(target.agent.id, target.queue);
                    const pill = agentTabStatusPill(
                      target.agent.status,
                      target.queue.length,
                      waits,
                    );
                    return (
                      <div
                        key={target.agent.id}
                        className={`pane-tab-row${isSelected ? " is-selected" : ""}`}
                        role="presentation"
                      >
                        <button
                          type="button"
                          role="option"
                          id={targetOptionDomId(target.agent.id)}
                          aria-selected={isSelected}
                          tabIndex={isSelected ? 0 : -1}
                          className="control-button pane-tab"
                          title={targetDetails(target)}
                          onClick={() => selectTarget(target.agent.id, "textarea")}
                        >
                          <span
                            className={agentTabStatusDotClass(target.agent.status, waits)}
                            aria-hidden="true"
                          />
                          <span className="pane-tab-content">
                            <span className="pane-tab-title">{target.title}</span>
                            {pill ? <small>{pill}</small> : null}
                          </span>
                        </button>
                      </div>
                    );
                  })}
                </div>
              );
            })}
          </div>
        )}
        {targets.length > 1 ? (
          <div className="global-task-launcher-targets-hint" aria-hidden="true">
            ⌃⇥ switches target
          </div>
        ) : null}
      </aside>

      <section className="global-task-launcher-compose" aria-label="Quick launch task">
        {selected && (hasExchange || queuedTurns.length > 0) ? (
          <div
            className="global-task-launcher-context"
            aria-label={[
              hasExchange ? "Last exchange with this tab" : null,
              queuedTurns.length > 0 ? `${queuedTurns.length} turns queued` : null,
            ]
              .filter(Boolean)
              .join("; ")}
          >
            <div className="global-task-launcher-context-label">{contextHeader}</div>
            {youLine ? (
              <div className="global-task-launcher-context-row" title={youLine.title}>
                <span className="global-task-launcher-context-prefix">You</span>
                <span className="global-task-launcher-context-text">{youLine.text}</span>
              </div>
            ) : null}
            {agentLine ? (
              <div className="global-task-launcher-context-row" title={agentLine.title}>
                <span className="global-task-launcher-context-prefix">Agent</span>
                <span className="global-task-launcher-context-text">{agentLine.text}</span>
              </div>
            ) : null}
            {queuedTurns.slice(0, 3).map((turn, index) => (
              <div
                key={index}
                className="global-task-launcher-context-row"
                title={turn.text}
              >
                <span className="global-task-launcher-context-prefix">{index + 1}</span>
                <span className="global-task-launcher-context-text">{turn.text}</span>
                {turn.waitFor ? (
                  <span className="global-task-launcher-context-wait">
                    waits for {turn.waitFor.label ?? "another tab"}
                  </span>
                ) : null}
              </div>
            ))}
            {queuedTurns.length > 3 ? (
              <div className="global-task-launcher-context-more">
                +{queuedTurns.length - 3} more
              </div>
            ) : null}
          </div>
        ) : null}
        <textarea
          ref={textareaRef}
          value={value}
          disabled={!selected && !loading}
          placeholder={loading ? "Loading agent tabs…" : "Describe a task…"}
          onChange={(event) => {
            valueTouchedRef.current = true;
            setValue(event.currentTarget.value);
          }}
          onKeyDown={(event) => {
            if (isComposerSubmitShortcut(event, requireCmdEnterToSend)) {
              event.preventDefault();
              submitDefault();
            }
          }}
        />

        {error ? <div className="global-task-launcher-error" role="alert">{error}</div> : null}

        <div className="global-task-launcher-actions">
          {permissionActions.map((action) => (
            <button
              key={action.id}
              type="button"
              className="control-button"
              disabled={submitting || !selected}
              onClick={() => {
                if (!selected) return;
                // Unlike a task submission, answering a permission prompt keeps
                // the window open and the typed draft intact, mirroring the
                // composer.
                setSubmitting(true);
                setError(null);
                void submitPaneInput(selected.pane.id, action.input)
                  .then(() => refresh())
                  .catch((cause) => setError(errorMessage(cause)))
                  .finally(() => setSubmitting(false));
              }}
            >
              {action.label}
            </button>
          ))}
          {parsedSlashCommand.kind !== "none" ? (
            <>
              {slashCanQueue ? (
                <button
                  type="button"
                  className="control-button"
                  disabled={submitting || !canFork}
                  title={!canFork ? FORK_REQUIREMENT_TITLE : undefined}
                  onClick={() => submit("send")}
                >
                  <span>{slashSubmitLabels?.now}</span>
                </button>
              ) : null}
              <button
                type="button"
                className="control-button"
                disabled={submitting || parsedSlashCommand.kind !== "ready" || !canFork}
                title={!canFork ? FORK_REQUIREMENT_TITLE : undefined}
                onClick={() => submit(slashCanQueue ? "queue" : "send")}
              >
                <span>
                  {slashCanQueue ? slashSubmitLabels?.queued : slashSubmitLabels?.immediate}
                </span>
                <ComposerSubmitShortcutGlyph
                  requireCmdEnter={requireCmdEnterToSend}
                  className="shortcut-hint"
                />
              </button>
            </>
          ) : (
            <>
              {canSend ? (
                <button
                  type="button"
                  className="control-button"
                  disabled={submitting || !hasValue}
                  onClick={() => submit("send")}
                >
                  <span>Send</span>
                  {submitShortcutWouldTargetSend ? (
                    <ComposerSubmitShortcutGlyph
                      requireCmdEnter={requireCmdEnterToSend}
                      className="shortcut-hint"
                    />
                  ) : null}
                </button>
              ) : null}
              <button
                type="button"
                className="control-button"
                disabled={submitting || !canSteer || !hasValue}
                onClick={() => submit("steer")}
                title={
                  canSteer
                    ? "Send now, interrupting the agent's current work"
                    : "Send Now is available while the agent is working"
                }
              >
                Send Now
              </button>
              {selected?.agent.paused ? (
                <button
                  type="button"
                  className="control-button queue-button"
                  disabled={submitting}
                  onClick={() => {
                    setSubmitting(true);
                    void unpauseAgent(selected.agent.id)
                      .then(() => refresh())
                      .catch((cause) => setError(errorMessage(cause)))
                      .finally(() => setSubmitting(false));
                  }}
                >
                  Unpause
                </button>
              ) : (
                <div className="global-task-launcher-queue-group queue-button-group">
                  <button
                    type="button"
                    className="control-button queue-button queue-button-main"
                    disabled={submitting || !canAppendQueue || !hasValue}
                    onClick={() => submit("queue")}
                  >
                    <span>Queue</span>
                    {submitShortcutWouldTargetQueue ? (
                      <ComposerSubmitShortcutGlyph
                        requireCmdEnter={requireCmdEnterToSend}
                        className="shortcut-hint"
                      />
                    ) : null}
                  </button>
                  <button
                    type="button"
                    className="control-button queue-menu-button"
                    aria-label="Queue options"
                    aria-haspopup="menu"
                    aria-expanded={queueMenuOpen}
                    title="Queue this turn to a fork, a new session, or after another terminal"
                    disabled={submitting || !selected || !hasValue}
                    onClick={() => setQueueMenuOpen((open) => !open)}
                  >
                    <ChevronDown size={14} aria-hidden="true" />
                  </button>
                  {queueMenuOpen ? (
                    <div className="global-task-launcher-queue-menu popover-surface" role="menu">
                      {QUEUE_DELIVERY_OPTIONS.map((option) => (
                        <button
                          key={option.label}
                          type="button"
                          role="menuitem"
                          className="menu-item composer-menu-item"
                          disabled={option.needsFork && !canFork}
                          title={
                            option.needsFork && !canFork
                              ? FORK_REQUIREMENT_TITLE
                              : option.title
                          }
                          onClick={() => {
                            if (!selected) return;
                            const text = value.trim();
                            void finishSubmission(() =>
                              queueDeliveryAgentTurn(
                                selected.agent.id,
                                text,
                                option.delivery,
                              ),
                            );
                          }}
                        >
                          {option.label}
                        </button>
                      ))}
                      {waitTargets.length > 0 ? (
                        <>
                          <div className="composer-menu-divider" role="separator" />
                          <div className="composer-menu-label wait-target-placeholder">
                            Queue after existing session…
                          </div>
                        </>
                      ) : null}
                      {waitTargets.map((target) => (
                        <button
                          key={target.agentId}
                          type="button"
                          role="menuitem"
                          className="menu-item wait-target-item"
                          title={target.label}
                          onClick={() => {
                            if (!selected) return;
                            const text = value.trim();
                            void finishSubmission(() =>
                              queueWaitAgentTurn(
                                selected.agent.id,
                                text,
                                target.agentId,
                                target.paneId,
                                target.label,
                              ),
                            );
                          }}
                        >
                          <span
                            className={waitTargetStatusDotClass(target)}
                            aria-hidden="true"
                          />
                          <span className="wait-target-title">{target.label}</span>
                          <span className="wait-target-status">
                            {waitTargetStatusLabel(target)}
                          </span>
                        </button>
                      ))}
                    </div>
                  ) : null}
                </div>
              )}
            </>
          )}
        </div>
      </section>
    </main>
  );
}
