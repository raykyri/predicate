import { useEffect } from "react";
import type { Dispatch, SetStateAction } from "react";
import {
  getThreadGraph,
  listAgents,
  listGroups,
  listPanes,
  listThreadGraphs,
  listenToEvents,
  markEventsListenerReady,
} from "../lib/api";
import {
  isAgentInfo,
  isGlobalDraft,
  isQueuedTurn,
  isTurn,
  reconcileReplacedTurns,
  reconcileThreadGraphs,
  transcriptHookEvent,
  upsertAgent,
  upsertThreadGraphs,
} from "../lib/appHelpers";
import { parseAppShortcutCommand, type AppShortcutCommand } from "../lib/appShortcuts";
import type { ExitPreflightRequest, PaneContextMenuState } from "../appTypes";
import type {
  AgentInfo,
  GlobalDraft,
  GroupInfo,
  PaneInfo,
  QmuxEvent,
  QueuedTurn,
  ShellAgentJobInfo,
  ThreadGraph,
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

// Trailing debounce for thread-graph refetches raised by turn events. Graphs
// back branch pickers and fork lineage — cosmetic relative to the timeline
// itself — so a few hundred milliseconds of staleness is invisible, while
// refetching per event burst was a full IPC round-trip plus a JSON diff of
// every graph several times a second during streaming.
const THREAD_GRAPH_REFRESH_DEBOUNCE_MS = 300;

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
  setThreadGraphs: Dispatch<SetStateAction<ThreadGraph[]>>;
  setTranscriptNoticeByAgent: Dispatch<SetStateAction<Record<string, string | null>>>;
  setShellJobByAgent: Dispatch<SetStateAction<Record<string, ShellAgentJobInfo>>>;
  setAgentQueuedTurns: (agentId: string, queuedTurns: QueuedTurn[]) => void;
  setGlobalDrafts: Dispatch<SetStateAction<GlobalDraft[]>>;
  // Resolves an agent id to its thread id (including the backend's synthetic
  // `thread-{agentId}` fallback), or null for an agent the app doesn't know yet.
  // Lets turn events refresh only the affected thread's graph instead of
  // refetching every graph in the workspace; an unresolvable agent falls back
  // to the full refetch.
  getAgentThreadId?: (agentId: string) => string | null;
  // True for agents that belong to research runs. The backend deliberately
  // writes no thread graph for them (research follow-ups branch through the
  // research tree, not the fork-lineage graph), so a graph refresh for one can
  // only miss — and a miss escalates to the full every-graph refetch, an
  // O(workspace history) fetch that fires on every research turn burst. Their
  // turn events must skip graph refreshes entirely.
  isResearchAgent?: (agentId: string) => boolean;
  refreshAgentTurnQueue: (agentId: string) => Promise<void>;
  refreshTranscriptOptions: (agentId: string) => Promise<void>;
  // Binds a browser-overlay URL to a pane (the backend emits the fully-formed URL).
  // `sandbox` marks token-bearing file-server URLs so the iframe is sandboxed.
  openBrowserOverlay: (paneId: string, url: string, sandbox?: boolean) => void;
  // Picks the next active pane when a pane closes, honoring split membership and
  // collapsed groups. Supplied by App so the pane.removed path selects consistently with
  // the user-initiated close path (forgetClosedPane).
  selectPaneAfterClose: (panes: PaneInfo[], closedPaneId: string) => string | null;
  // Fired once the single backend subscription is live, so panes can safely flush
  // their pre-attach output backlog (attachPane) without dropping cold-start bytes.
  onEventsReady: () => void;
  onAgentSpawned?: (agent: AgentInfo, paneId: string | null, source: string | null) => void;
  onAgentPromptSubmitted?: (agentId: string, prompt: string) => void;
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
    setThreadGraphs,
    setTranscriptNoticeByAgent,
    setShellJobByAgent,
    setAgentQueuedTurns,
    setGlobalDrafts,
    getAgentThreadId,
    isResearchAgent,
    refreshAgentTurnQueue,
    refreshTranscriptOptions,
    openBrowserOverlay,
    selectPaneAfterClose: selectPaneAfterCloseWithContext,
    onEventsReady,
    onAgentSpawned,
    onAgentPromptSubmitted,
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
    let threadGraphRefreshSeq = 0;
    // Turn events arrive in bursts — every appended line schedules a refresh,
    // and the transcript tail delivers a fresh burst every few hundred ms for
    // as long as an agent streams. A microtask-level collapse still refetched
    // (and re-serialized, re-parsed, and JSON-diffed) every thread graph once
    // per burst. Debounce with a trailing timer instead: the timeline renders
    // from `turns` directly, so graph freshness is not latency-critical, and
    // one refetch per quiet window replaces several per second. The seq guard
    // still drops any stale response that loses a race.
    let threadGraphRefreshTimer: number | null = null;
    // Agents whose turn activity arrived since the last flush. When every one of
    // them resolves to a thread id, the flush fetches just those threads' graphs;
    // graphs retain full transcript nodes, so refetching all of them per burst
    // cost O(total workspace history) rather than O(the streaming thread).
    const dirtyGraphAgentIds = new Set<string>();
    // Set when a refresh request carried no agent id: the flush must then fall
    // back to the full refetch even if resolvable agents are dirty too, or the
    // anonymous event's thread would silently miss its refresh.
    let graphRefreshNeedsFull = false;
    // Per-thread fetch sequence for targeted refreshes (see flush below).
    const threadGraphFetchSeqByThread = new Map<string, number>();
    // Full refetch of every graph — hydration-grade recovery. Interlocked with
    // the targeted path in both directions: a targeted apply bumps
    // threadGraphRefreshSeq so a slower full response can't roll that thread
    // back, and a full apply bumps every per-thread seq so a slower targeted
    // response can't roll back the fresher full snapshot.
    const refreshAllThreadGraphs = () => {
      const seq = (threadGraphRefreshSeq += 1);
      void listThreadGraphs()
        .then((graphs) => {
          if (!disposed && seq === threadGraphRefreshSeq) {
            for (const [threadId, threadSeq] of threadGraphFetchSeqByThread) {
              threadGraphFetchSeqByThread.set(threadId, threadSeq + 1);
            }
            // Reuse prior graph objects when their content is unchanged so
            // the per-agent turn-info cache (keyed on graph identity) holds.
            setThreadGraphs((current) => reconcileThreadGraphs(current, graphs));
          }
        })
        .catch(() => undefined);
    };
    const refreshThreadGraphs = (agentId?: string | null) => {
      if (agentId && isResearchAgent?.(agentId)) {
        // A research run's turn events (its start burst — the initial
        // transcript reset — and its settle flush are the densest) must not
        // schedule graph work: no graph will ever exist for it, and the miss
        // path's full refetch was a main-thread beachball at exactly those
        // moments. This also covers a settled run's trailing turn events,
        // which arrive after remove_pane dropped the agent and would
        // otherwise resolve no thread id and force the full refetch.
        return;
      }
      if (agentId) {
        dirtyGraphAgentIds.add(agentId);
      } else {
        graphRefreshNeedsFull = true;
      }
      if (threadGraphRefreshTimer !== null) {
        return;
      }
      threadGraphRefreshTimer = window.setTimeout(() => {
        threadGraphRefreshTimer = null;
        if (disposed) {
          return;
        }
        const dirtyAgents = [...dirtyGraphAgentIds];
        dirtyGraphAgentIds.clear();
        const needsFull = graphRefreshNeedsFull;
        graphRefreshNeedsFull = false;
        const threadIds =
          !needsFull && getAgentThreadId && dirtyAgents.length > 0
            ? dirtyAgents.map((dirtyAgentId) => getAgentThreadId(dirtyAgentId))
            : null;
        if (threadIds && threadIds.every((threadId): threadId is string => threadId !== null)) {
          const fetches = [...new Set(threadIds)].map((threadId) => {
            const seq = (threadGraphFetchSeqByThread.get(threadId) ?? 0) + 1;
            threadGraphFetchSeqByThread.set(threadId, seq);
            return getThreadGraph(threadId)
              .then((graph) => ({ threadId, seq, graph }))
              .catch(() => null);
          });
          // One state commit for the whole batch — per-promise applies would
          // re-render the app once per streaming thread per flush. Each result
          // still applies only if its per-thread seq is current, so a slower
          // duplicate fetch or an interleaved full refresh wins by recency.
          void Promise.all(fetches).then((results) => {
            if (disposed) {
              return;
            }
            const updates: ThreadGraph[] = [];
            let missing = false;
            for (const result of results) {
              if (result === null) {
                continue;
              }
              if (result.graph === null) {
                // The backend has no graph under this id — the resolved thread
                // id is wrong or the record is missing. Recover with the full
                // refetch rather than silently never refreshing this thread.
                missing = true;
              } else if (threadGraphFetchSeqByThread.get(result.threadId) === result.seq) {
                updates.push(result.graph);
              }
            }
            if (updates.length > 0) {
              threadGraphRefreshSeq += 1;
              // Content-identical refetches keep the prior graph object so the
              // per-agent turn-info cache (keyed on graph identity) holds.
              setThreadGraphs((current) => upsertThreadGraphs(current, updates));
            }
            if (missing) {
              refreshAllThreadGraphs();
            }
          });
          return;
        }
        // An event without a resolvable agent (or no resolver supplied): fall
        // back to the full refetch — correctness recovery over efficiency.
        refreshAllThreadGraphs();
      }, THREAD_GRAPH_REFRESH_DEBOUNCE_MS);
    };

    const handleEvent = (event: QmuxEvent) => {
      if (event.type.startsWith("research.")) {
        onResearchChanged?.(event);
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
      if (event.type === "pane.cwd_changed" && event.paneId) {
        // A shell tab reported a directory change (the user cd'd). The backend has
        // already persisted it for restart recovery; patch the live pane so the tab
        // path and context-menu working dir track the current directory instead of
        // lagging at the spawn-time cwd until the next full pane-list load.
        const cwdPaneId = event.paneId;
        const nextCwd = event.payload.cwd;
        if (typeof nextCwd === "string") {
          setPanes((current) =>
            current.map((pane) =>
              pane.id === cwdPaneId ? { ...pane, cwd: nextCwd } : pane,
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
        setExitPreflightRequest((current) => ({
          paneCount,
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
          openBrowserOverlay(event.paneId, url, event.payload.sandbox === true);
        }
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
          (event.payload.source === "queue" ||
            event.payload.source === "research" ||
            event.payload.source === "encyclopedia"))
      ) {
        // The fork — or a queue-dispatched new-session, research-root, or
        // encyclopedia-update spawn —
        // created a new pane backend-side with no frontend caller holding it;
        // refetch the ordered list so the nested tab appears (with its depth)
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
        setTurns((current) => reconcileReplacedTurns(current, agentId, replacementTurns));
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
      if (threadGraphRefreshTimer !== null) {
        clearTimeout(threadGraphRefreshTimer);
      }
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}
