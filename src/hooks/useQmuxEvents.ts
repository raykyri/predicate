import { useEffect } from "react";
import type { Dispatch, SetStateAction } from "react";
import {
  acknowledgeInterfaceHealthProbe,
  listAgents,
  listGroups,
  listPanes,
  listenToEvents,
  markEventsListenerReady,
} from "../lib/api";
import {
  agentEventAffectsThinkingState,
  isAgentInfo,
  isGlobalDraft,
  isQueuedTurn,
  isTurn,
  reconcileReplacedTurns,
  transcriptHookEvent,
  upsertAgent,
} from "../lib/appHelpers";
import { parseAppShortcutCommand, type AppShortcutCommand } from "../lib/appShortcuts";
import type { ExitPreflightRequest, PaneContextMenuState } from "../appTypes";
import type {
  ActiveWorkspace,
  AgentInfo,
  GlobalDraft,
  GroupInfo,
  PaneInfo,
  PaneSplitInfo,
  QmuxEvent,
  QueuedTurn,
  ShellAgentJobInfo,
  TranscriptHookEvent,
  Turn,
} from "../types";

// How long events accumulate before the batch is handled. A busy agent emits
// bursts of hook/status/turn events (dozens per second), and handling each in
// its own listener callback commits a separate React render of the whole app —
// which is what makes typing lag while an agent streams. Coalescing is
// trailing-only: every event (including the first of a burst) waits for the
// window, then the whole batch is processed in arrival order in one synchronous
// block, which React commits as a single render. Under a sustained stream that
// caps handling at one render per window; the old leading-edge variant handled
// the first event of every burst synchronously and committed roughly two
// renders per window under exactly the load the coalescing was built for.
// Interactive events (terminal shortcuts, paste requests) share the window and
// so run up to one frame late — kept deliberately, since reordering them ahead
// of queued pane/agent events would let a shortcut act on state the queued
// events are about to change, and one frame is imperceptible for those actions.
const EVENT_COALESCE_MS = 16;

// Mirror the backend's per-agent turn cap (MAX_TURNS_PER_AGENT in state.rs). The
// backend only ever holds the most recent N turns per agent, but the frontend
// appended to its global turns array forever — a long-lived session grew memory and
// per-render cost without bound. Keep the newest N per agent to match.
const MAX_TURNS_PER_AGENT = 200;

// The backend event stream drives most of the app's live state. This hook owns
// the single global subscription: it is intentionally set up once (empty deps),
// so every callback it touches is passed in and captured at first render,
// matching how the inline effect behaved before it was extracted. State setters
// from useState are stable, and the three helper callbacks read through refs
// internally, so the first-render capture stays correct.
export interface UseQmuxEventsHandlers {
  // Records a transcript hook event for the copy-as-JSON export. Nothing renders
  // hook events, so the store lives outside React state (see App) and appending
  // must never trigger a render.
  appendHookEvent: (event: TranscriptHookEvent) => void;
  setPanes: Dispatch<SetStateAction<PaneInfo[]>>;
  setActivePaneId: Dispatch<SetStateAction<string | null>>;
  setPaneContextMenu: Dispatch<SetStateAction<PaneContextMenuState | null>>;
  setExitPreflightRequest: Dispatch<SetStateAction<ExitPreflightRequest | null>>;
  setAgents: Dispatch<SetStateAction<AgentInfo[]>>;
  setGroups: Dispatch<SetStateAction<GroupInfo[]>>;
  // Tracks which agents are actively working, for the transcript "Working…"
  // indicator. Only live transitions into a working status flip it on, so an
  // agent restored into a working status never falsely shows it (see below).
  setThinkingAgentIds: Dispatch<SetStateAction<Set<string>>>;
  setTurns: Dispatch<SetStateAction<Turn[]>>;
  setTranscriptNoticeByAgent: Dispatch<SetStateAction<Record<string, string | null>>>;
  setShellJobByAgent: Dispatch<SetStateAction<Record<string, ShellAgentJobInfo>>>;
  setAgentQueuedTurns: (agentId: string, queuedTurns: QueuedTurn[]) => void;
  setGlobalDrafts: Dispatch<SetStateAction<GlobalDraft[]>>;
  // Thread graphs are demand-loaded by App. Hidden or parked agents still emit
  // turn events, but those events must not populate an ever-growing graph cache.
  shouldRefreshAgentThreadGraph?: (agentId: string) => boolean;
  // App owns graph request sequencing alongside initial hydration. Keeping both
  // paths behind one coordinator prevents a slow initial read from overwriting a
  // newer event-driven read of the same thread.
  onAgentThreadGraphDirty?: (agentId: string) => void;
  // Turn snapshots follow the same surface ownership policy. App can rehydrate
  // the backend's bounded window when a hidden agent becomes visible, so retaining
  // every hidden agent's live window here would only turn the frontend into an
  // app-lifetime cache again.
  shouldRetainAgentTurns?: (agentId: string) => boolean;
  // True for agents that belong to research runs. The backend deliberately
  // writes no thread graph for them (research follow-ups branch through the
  // research tree, not the fork-lineage graph), so a graph refresh can only
  // miss. Their dense turn-event bursts must skip graph work entirely.
  isResearchAgent?: (agentId: string) => boolean;
  refreshAgentTurnQueue: (agentId: string) => Promise<void>;
  refreshTranscriptOptions: (agentId: string) => Promise<void>;
  // Binds a browser-overlay URL to a pane (the backend emits the fully-formed URL).
  openBrowserOverlay: (
    paneId: string,
    url: string,
    sandbox?: boolean,
    artifactId?: string | null,
  ) => void;
  // Picks the next active pane when a pane closes, honoring split membership and
  // collapsed groups. Supplied by App so the pane.removed path selects consistently with
  // the user-initiated close path (forgetClosedPane).
  selectPaneAfterClose: (panes: PaneInfo[], closedPaneId: string) => string | null;
  // Fired once the single backend subscription is live, so panes can safely flush
  // their pre-attach output backlog (attachPane) without dropping cold-start bytes.
  onEventsReady: () => void;
  onAgentSpawned?: (agent: AgentInfo, paneId: string | null, source: string | null) => void;
  onAgentPromptSubmitted?: (agentId: string, prompt: string) => void;
  /** Artifact tray: `artifact.added` / `artifact.removed`. App owns the state. */
  onArtifactEvent?: (event: QmuxEvent) => void;
  onPaneFocusRequested?: (paneId: string) => void;
  onPaneSplitsChanged?: (splits: PaneSplitInfo[]) => void;
  onTerminalSearchRequested?: (paneId: string) => void;
  onTerminalPasteRequested?: (paneId: string, text: string | null) => void;
  onTerminalUserInput?: (paneId: string) => void;
  onTerminalActivated?: (paneId: string) => void;
  onTerminalShortcut?: (
    paneId: string,
    command: AppShortcutCommand,
    repeat: boolean,
  ) => void;
  onAppShortcut?: (command: AppShortcutCommand, repeat: boolean) => void;
  onTerminalCommandModifier?: (paneId: string, active: boolean) => void;
  onTerminalOpenUrl?: (paneId: string, url: string) => void;
  onTerminalTitleChanged?: (paneId: string, title: string) => void;
  onResearchChanged?: (event: QmuxEvent) => void;
  onUserNotificationRequested?: (event: QmuxEvent) => void;
  onNotificationOpenPane?: (paneId: string) => void;
}

function stringField(value: unknown, field: string): string | null {
  if (typeof value !== "object" || value === null) {
    return null;
  }
  const fieldValue = (value as Record<string, unknown>)[field];
  return typeof fieldValue === "string" ? fieldValue : null;
}

function agentPromptSubmittedText(payload: Record<string, unknown>): string | null {
  const hookPayload = payload.payload;
  return stringField(hookPayload, "prompt") ?? stringField(hookPayload, "input");
}

export function useQmuxEvents(handlers: UseQmuxEventsHandlers) {
  const {
    appendHookEvent,
    setPanes,
    setActivePaneId,
    setPaneContextMenu,
    setExitPreflightRequest,
    setAgents,
    setGroups,
    setThinkingAgentIds,
    setTurns,
    setTranscriptNoticeByAgent,
    setShellJobByAgent,
    setAgentQueuedTurns,
    setGlobalDrafts,
    shouldRefreshAgentThreadGraph,
    onAgentThreadGraphDirty,
    shouldRetainAgentTurns,
    isResearchAgent,
    refreshAgentTurnQueue,
    refreshTranscriptOptions,
    openBrowserOverlay,
    selectPaneAfterClose: selectPaneAfterCloseWithContext,
    onEventsReady,
    onAgentSpawned,
    onAgentPromptSubmitted,
    onArtifactEvent,
    onPaneFocusRequested,
    onPaneSplitsChanged,
    onTerminalSearchRequested,
    onTerminalPasteRequested,
    onTerminalUserInput,
    onTerminalActivated,
    onTerminalShortcut,
    onAppShortcut,
    onTerminalCommandModifier,
    onTerminalOpenUrl,
    onTerminalTitleChanged,
    onResearchChanged,
    onUserNotificationRequested,
    onNotificationOpenPane,
  } = handlers;

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    // Sequences full agent-list refetches so a slow response can't overwrite a
    // newer snapshot. Only bumped on events that don't already carry the agent.
    let agentRefreshSeq = 0;
    // Same idea for pane-list refetches (a fork adds a pane backend-side).
    let panesRefreshSeq = 0;
    let groupsRefreshSeq = 0;
    const refreshThreadGraphs = (agentId?: string | null) => {
      if (
        !agentId ||
        isResearchAgent?.(agentId) ||
        shouldRefreshAgentThreadGraph?.(agentId) === false
      ) {
        // A research run's turn events (its start burst — the initial
        // transcript reset — and its settle flush are the densest) must not
        // schedule graph work: no graph will ever exist for it. Hidden agents
        // likewise stay on the bounded turn fallback until their surface asks
        // for the graph.
        return;
      }
      onAgentThreadGraphDirty?.(agentId);
    };

    const handleEvent = (event: QmuxEvent) => {
      if (event.type === "app.interface_health_probe") {
        const generation = event.payload.generation;
        if (typeof generation === "number" && Number.isSafeInteger(generation)) {
          // Waiting for two animation frames makes the acknowledgement prove
          // more than a live JavaScript event loop: WebKit must still be able
          // to schedule a paint after suspension/GPU-process churn. If rAF is
          // wedged, the native watchdog reloads the document instead.
          requestAnimationFrame(() => {
            requestAnimationFrame(() => {
              void acknowledgeInterfaceHealthProbe(generation).catch(() => undefined);
            });
          });
        }
      }
      if (event.type.startsWith("research.")) {
        onResearchChanged?.(event);
      }
      if (event.type === "app.notification_requested") {
        onUserNotificationRequested?.(event);
      }
      if (event.type === "app.notification_open_pane" && event.paneId) {
        onNotificationOpenPane?.(event.paneId);
      }
      const hookEvent = transcriptHookEvent(event);
      if (hookEvent) {
        appendHookEvent(hookEvent);
      }
      if (event.type === "pane.removed" && event.paneId) {
        const exitedPaneId = event.paneId;
        setPanes((current) => {
          const nextPanes = current.filter((pane) => pane.id !== exitedPaneId);
          setActivePaneId((currentActivePaneId) => {
            if (currentActivePaneId !== exitedPaneId) {
              return currentActivePaneId;
            }
            return selectPaneAfterCloseWithContext(current, exitedPaneId);
          });
          return nextPanes;
        });
        setPaneContextMenu((current) => (current?.paneId === exitedPaneId ? null : current));
      }
      if (event.type === "pane.focus_requested" && event.paneId) {
        onPaneFocusRequested?.(event.paneId);
      }
      if (event.type === "pane.splits_changed" && Array.isArray(event.payload.splits)) {
        onPaneSplitsChanged?.(event.payload.splits as PaneSplitInfo[]);
      }
      if (event.type === "pane.created" || event.type === "pane.renamed") {
        const seq = (panesRefreshSeq += 1);
        void listPanes()
          .then((latest) => {
            if (!disposed && seq === panesRefreshSeq) {
              setPanes(latest);
            }
          })
          .catch(() => undefined);
      }
      if (event.type === "pane.cwd_changed" && event.paneId) {
        // A shell tab reported a directory change (the user cd'd). The backend has
        // already persisted it for restart recovery; patch the live pane so the tab
        // path and context-menu working dir track the current directory instead of
        // lagging at the spawn-time cwd until the next full pane-list load. The
        // payload also carries the freshly resolved workspace observation so the
        // worktree badge updates in the same step.
        const cwdPaneId = event.paneId;
        const nextCwd = event.payload.cwd;
        if (typeof nextCwd === "string") {
          const rawWorkspace = event.payload.activeWorkspace;
          const nextWorkspace =
            rawWorkspace && typeof rawWorkspace === "object"
              ? (rawWorkspace as ActiveWorkspace)
              : null;
          setPanes((current) =>
            current.map((pane) =>
              pane.id === cwdPaneId
                ? { ...pane, cwd: nextCwd, activeWorkspace: nextWorkspace }
                : pane,
            ),
          );
        }
      }
      if (event.type === "terminal.title_changed" && event.paneId) {
        const title = stringField(event.payload, "title");
        if (title !== null) {
          onTerminalTitleChanged?.(event.paneId, title);
        }
      }
      if (event.agentId && event.type === "agent.shell_job_state_changed") {
        const agentId = event.agentId;
        const job = event.payload.job;
        if (typeof job === "object" && job !== null) {
          const candidate = job as Partial<ShellAgentJobInfo>;
          if (
            candidate.agentId === agentId &&
            typeof candidate.jobId === "string" &&
            typeof candidate.paneId === "string" &&
            (candidate.state === "foreground" ||
              candidate.state === "backgrounded" ||
              candidate.state === "stopped")
          ) {
            setShellJobByAgent((current) => ({
              ...current,
              [agentId]: candidate as ShellAgentJobInfo,
            }));
          }
        }
      }
      if (event.agentId && event.type === "agent.shell_job_removed") {
        const agentId = event.agentId;
        const jobId = stringField(event.payload, "jobId");
        setShellJobByAgent((current) => {
          if (!jobId || current[agentId]?.jobId !== jobId) {
            return current;
          }
          const next = { ...current };
          delete next[agentId];
          return next;
        });
      }
      if (event.type === "terminal.search_requested" && event.paneId) {
        onTerminalSearchRequested?.(event.paneId);
      }
      if (event.type === "terminal.paste_requested" && event.paneId) {
        onTerminalPasteRequested?.(event.paneId, stringField(event.payload, "text"));
      }
      if (event.type === "terminal.user_input" && event.paneId) {
        onTerminalUserInput?.(event.paneId);
      }
      if (event.type === "terminal.activated" && event.paneId) {
        onTerminalActivated?.(event.paneId);
      }
      if (event.type === "terminal.shortcut" && event.paneId) {
        const command = parseAppShortcutCommand(
          event.payload.command,
          event.payload.tabIndex,
        );
        if (command !== null) {
          onTerminalShortcut?.(event.paneId, command, event.payload.repeat === true);
        }
      }
      if (event.type === "app.shortcut") {
        const command = parseAppShortcutCommand(
          event.payload.command,
          event.payload.tabIndex,
        );
        if (command !== null) {
          onAppShortcut?.(command, event.payload.repeat === true);
        }
      }
      if (event.type === "terminal.command_modifier_changed" && event.paneId) {
        onTerminalCommandModifier?.(event.paneId, event.payload.active === true);
      }
      if (event.type === "terminal.open_url" && event.paneId) {
        const url = stringField(event.payload, "url");
        if (url !== null) {
          onTerminalOpenUrl?.(event.paneId, url);
        }
      }
      if (event.type === "app.exit_confirmation_requested") {
        const paneCount =
          typeof event.payload.paneCount === "number" ? event.payload.paneCount : 1;
        const researchRunCount =
          typeof event.payload.researchRunCount === "number"
            ? event.payload.researchRunCount
            : 0;
        setExitPreflightRequest((current) => ({
          paneCount,
          researchRunCount,
          nonce: (current?.nonce ?? 0) + 1,
        }));
      }
      if (
        event.type.startsWith("agent.") &&
        event.type !== "agent.shell_job_state_changed" &&
        event.type !== "agent.shell_job_removed"
      ) {
        // Status events now carry the updated agent: apply it surgically so a busy
        // agent's stream of hook events doesn't refetch and replace the entire list
        // (with the re-renders and ordering hazards that caused). Events without an
        // agent fall back to a sequenced refetch.
        const updatedAgent = event.payload.agent;
        if (isAgentInfo(updatedAgent)) {
          setAgents((current) => upsertAgent(current, updatedAgent));
          if (event.type === "agent.spawned") {
            onAgentSpawned?.(
              updatedAgent,
              event.paneId ?? updatedAgent.paneId ?? null,
              stringField(event.payload, "source"),
            );
          }
          // Light up "Working…" only on a *live* transition into a working
          // status. The boot snapshot loads agents via setAgents(list) (the
          // else-branch below), which never touches this set, so a stale
          // working status restored from disk can't trigger it. "agent.recovered"
          // is excluded too: a recovered agent is waiting for input, not working,
          // even if it momentarily carries a working status.
          // Workspace-only refreshes carry a full AgentInfo so the display can
          // update surgically, but they are not lifecycle activity. In
          // particular, a sibling shell prompt must not make a restored agent
          // with a stale `running` status light up as "Working…".
          if (agentEventAffectsThinkingState(event.type)) {
            const working =
              updatedAgent.status === "running" || updatedAgent.status === "starting";
            setThinkingAgentIds((prev) => {
              const shouldThink = working && event.type !== "agent.recovered";
              if (shouldThink === prev.has(updatedAgent.id)) {
                return prev;
              }
              const next = new Set(prev);
              if (shouldThink) {
                next.add(updatedAgent.id);
              } else {
                next.delete(updatedAgent.id);
              }
              return next;
            });
          }
        } else {
          const seq = (agentRefreshSeq += 1);
          void listAgents()
            .then((list) => {
              if (!disposed && seq === agentRefreshSeq) {
                setAgents(list);
              }
            })
            .catch(() => undefined);
        }
      }
      if (event.agentId && event.type === "agent.prompt_submitted") {
        const prompt = agentPromptSubmittedText(event.payload);
        if (prompt) {
          onAgentPromptSubmitted?.(event.agentId, prompt);
        }
      }
      if (event.type === "browser.open" && event.paneId) {
        const url = event.payload.url;
        if (typeof url === "string") {
          const artifactId = event.payload.artifactId;
          openBrowserOverlay(
            event.paneId,
            url,
            event.payload.sandbox === true,
            typeof artifactId === "string" ? artifactId : null,
          );
        }
      }
      if (event.type === "artifact.added" || event.type === "artifact.removed") {
        onArtifactEvent?.(event);
      }
      if (
        event.type === "group.created" ||
        event.type === "group.updated" ||
        event.type === "group.removed"
      ) {
        const seq = (groupsRefreshSeq += 1);
        void listGroups()
          .then((latest) => {
            if (!disposed && seq === groupsRefreshSeq) {
              setGroups(latest);
            }
          })
          .catch(() => undefined);
      }
      if (
        event.type === "agent.forked" ||
        (event.type === "agent.spawned" &&
          event.payload.source !== "shell")
      ) {
        // The fork — or a queue-dispatched new-session or research-root spawn —
        // created a new pane backend-side with no frontend caller holding it;
        // refetch the ordered list so the newly placed tab appears
        // without stealing focus from the source.
        const seq = (panesRefreshSeq += 1);
        void listPanes()
          .then((latest) => {
            if (!disposed && seq === panesRefreshSeq) {
              setPanes(latest);
            }
          })
          .catch(() => undefined);
      }
      if (
        event.agentId &&
        (event.type === "agent.turn_queued" ||
          event.type === "agent.queued_turn_sent" ||
          event.type === "agent.queued_turn_removed" ||
          event.type === "agent.queued_turn_reordered" ||
          event.type === "agent.unpaused" ||
          event.type === "agent.queue_error")
      ) {
        const queuedTurns = Array.isArray(event.payload.queuedTurns)
          ? event.payload.queuedTurns.filter(isQueuedTurn)
          : null;
        if (queuedTurns) {
          setAgentQueuedTurns(event.agentId, queuedTurns);
        } else {
          void refreshAgentTurnQueue(event.agentId).catch(() => undefined);
        }
      }
      if (event.type === "drafts.changed") {
        const drafts = event.payload.drafts;
        if (Array.isArray(drafts) && drafts.every(isGlobalDraft)) {
          setGlobalDrafts(drafts);
        }
      }
      if (event.type === "turn.appended") {
        const turn = event.payload.turn;
        if (isTurn(turn)) {
          if (shouldRetainAgentTurns?.(turn.agentId) !== false) {
            setTurns((current) => {
              const existingIndex = current.findIndex((existing) => existing.id === turn.id);
              if (existingIndex !== -1) {
                // Positional turn ids can be reused across a transcript
                // rewrite/rebind, so a same-id append carries the id's newest
                // content and belongs at the tail. An identical re-delivery
                // keeps the array (and downstream memos) untouched.
                if (JSON.stringify(current[existingIndex]) === JSON.stringify(turn)) {
                  return current;
                }
                return [...current.filter((_, index) => index !== existingIndex), turn];
              }
              const next = [...current, turn];
              const agentTurnCount = next.reduce(
                (count, existing) => (existing.agentId === turn.agentId ? count + 1 : count),
                0,
              );
              if (agentTurnCount <= MAX_TURNS_PER_AGENT) {
                return next;
              }
              // Over the cap: drop the oldest turns for this agent (the earliest matches in
              // arrival order) so the global array can't grow without bound.
              let toDrop = agentTurnCount - MAX_TURNS_PER_AGENT;
              return next.filter((existing) => {
                if (toDrop > 0 && existing.agentId === turn.agentId) {
                  toDrop -= 1;
                  return false;
                }
                return true;
              });
            });
          }
          refreshThreadGraphs(turn.agentId);
        }
      }
      if (event.type === "turn.updated" && event.payload.reset) {
        const agentId = event.agentId;
        // A reset can carry more than the per-agent cap (the backend truncates its
        // stored copy but emits the full parsed window). Keep the newest N here too,
        // so the fallback timeline holds a stable-size window instead of flipping
        // between a full list on reset and a capped one on the next append.
        const replacementTurns = (
          Array.isArray(event.payload.turns) ? event.payload.turns.filter(isTurn) : []
        ).slice(-MAX_TURNS_PER_AGENT);
        // Reuse prior turn objects for content-identical replacements so the
        // per-agent turn caches (and per-message memos) hold across a reset;
        // see reconcileReplacedTurns.
        if (!agentId || shouldRetainAgentTurns?.(agentId) !== false) {
          setTurns((current) => reconcileReplacedTurns(current, agentId, replacementTurns));
        }
        refreshThreadGraphs(agentId);
      }
      if (
        event.agentId &&
        (event.type === "transcript.notice" || event.type === "transcript.error")
      ) {
        const agentId = event.agentId;
        // transcript.error carries `error`; transcript.notice carries `message`
        // (null/absent means the tail recovered, so the notice is cleared).
        const message =
          event.type === "transcript.error"
            ? typeof event.payload.error === "string"
              ? event.payload.error
              : "Failed to load transcript"
            : typeof event.payload.message === "string"
              ? event.payload.message
              : null;
        setTranscriptNoticeByAgent((current) => ({ ...current, [agentId]: message }));
        // A notice usually follows a recovery/rotation; refresh the picker so the
        // active session and any new candidates are reflected.
        void refreshTranscriptOptions(agentId).catch(() => undefined);
      }
      if (event.agentId && event.type === "agent.transcript_recovered") {
        void refreshTranscriptOptions(event.agentId).catch(() => undefined);
      }
    };

    const pendingEvents: QmuxEvent[] = [];
    let coalesceTimer: number | null = null;
    const flushPendingEvents = () => {
      coalesceTimer = null;
      if (disposed || pendingEvents.length === 0) {
        return;
      }
      const batch = pendingEvents.splice(0, pendingEvents.length);
      // Every setState across the batch runs in this one synchronous block, so
      // React commits a single render for the whole burst.
      for (const event of batch) {
        handleEvent(event);
      }
    };

    void listenToEvents((event) => {
      if (disposed) {
        return;
      }
      pendingEvents.push(event);
      if (coalesceTimer === null) {
        coalesceTimer = window.setTimeout(flushPendingEvents, EVENT_COALESCE_MS);
      }
    }).then((cleanup) => {
      if (disposed) {
        cleanup();
      } else {
        unlisten = cleanup;
        // Unblock the native shortcut classifiers: from here on an emitted
        // terminal.shortcut / app.shortcut event actually reaches this hook.
        void markEventsListenerReady().catch(() => undefined);
        onEventsReady();
      }
    });

    return () => {
      disposed = true;
      if (coalesceTimer !== null) {
        clearTimeout(coalesceTimer);
      }
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
