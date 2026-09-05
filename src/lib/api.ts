import { trackRemoteStartup, recordRemoteStartup, reconcileRemoteReservation, forgetRemoteStartup } from "./remoteStartup";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { JournalEntry, RecentActivityPage } from "./journal";
import type { PaneLayoutItem } from "./paneTree";
import type { ResearchFolderState } from "./researchFolders";
import type { WorktreeLocation } from "./settings";
import type { CompletionSoundId } from "./completionSounds";
import {
  HumanBrowserLifecycleQueue,
  retryHumanBrowserLifecycle,
} from "./humanBrowserLifecycleQueue";
import type {
  PublicationBinding,
  PublicationProposal,
  PublishPublicationRequest,
  PublishingAuthPollResult,
  PublishingAuthStatus,
  PublishingDeviceAuthorization,
  SyncPublicationRequest,
} from "./publication";
import type {
  AgentInfo,
  AgentDeliveryDebugInfo,
  ArtifactInfo,
  ConversationHistorySnapshot,
  ClaudeSkill,
  ConversationHistoryEntry,
  ConversationHistoryLaunchRequest,
  GlobalDraft,
  GlobalTaskLauncherHotkey,
  GlobalTaskLauncherSetting,
  GroupInfo,
  HomeTurnHistoryPage,
  InitialPaneSize,
  MessageAnchor,
  MoveQueuedAgentTurnResult,
  PaneActivity,
  PaneInfo,
  PaneSplitInfo,
  QmuxEvent,
  PromptLibrary,
  PromptScope,
  QueuedTurn,
  QueuedTurnDelivery,
  SavedPrompt,
  ShellAgentJobInfo,
  RemoveQueuedAgentTurnResult,
  ReorderQueuedAgentTurnResult,
  ResearchBranchRemoval,
  RecentActivityCursor,
  RecentResearchQueryCursor,
  RecentResearchQueryPage,
  ResearchHighlight,
  ResearchHighlightAnchor,
  ResearchTree,
  ResearchTreeDetail,
  ResearchTreeSummary,
  ResearchNode,
  ResearchNodeContent,
  UpdateResearchDocumentResult,
  SendNextQueuedAgentTurnResult,
  RuntimeConfig,
  RemoteChoice,
  RemoteProbeResult,
  SavedRemote,
  SpawnAgentRequest,
  SubmitAgentTurnMode,
  SubmitAgentTurnResult,
  TranscriptOption,
  ThreadGraph,
  Turn,
  WorktreeStatus,
} from "../types";

export function getRuntimeConfig() {
  return invoke<RuntimeConfig>("get_runtime_config");
}

export function listSshConfigAliases() {
  return invoke<string[]>("list_ssh_config_aliases");
}

export function upsertRemote(id: string, remote: SavedRemote) {
  return invoke<RemoteChoice[]>("upsert_remote", { id, remote });
}

export function deleteRemote(id: string) {
  return invoke<RemoteChoice[]>("delete_remote", { id });
}

export function probeRemote(remote: SavedRemote) {
  return invoke<RemoteProbeResult>("probe_remote", { remote });
}

export function probeAgentAdapters(options?: { groupId?: string | null; force?: boolean }) {
  return invoke<RuntimeConfig["adapters"]>("probe_agent_adapters", {
    groupId: options?.groupId ?? null,
    force: options?.force ?? false,
  });
}

/** Plays one catalogued completion sound through the native AppKit bridge. */
export function playCompletionSound(soundId: CompletionSoundId) {
  return invoke<void>("completion_sound_play", { soundId });
}

/** Keeps the reload-safe backend lifecycle player aligned with Display settings. */
export function setCompletionSound(soundId: CompletionSoundId) {
  return invoke<void>("completion_sound_set", { soundId });
}

export interface NotificationPermissionInfo {
  supported: boolean;
  status: "NotDetermined" | "Denied" | "Authorized" | "Provisional" | "Ephemeral" | "Unknown" | "Unavailable";
}

export function getNotificationPermission() {
  return invoke<NotificationPermissionInfo>("notification_permission_status");
}

export function requestNotificationPermission() {
  return invoke<NotificationPermissionInfo>("notification_request_permission");
}

export function listShellAgentJobs() {
  return invoke<ShellAgentJobInfo[]>("list_shell_agent_jobs");
}

export function listConversationHistory() {
  return invoke<ConversationHistoryEntry[]>("list_conversation_history");
}

export function launchConversationHistory(request: ConversationHistoryLaunchRequest) {
  return invoke<PaneInfo>("launch_conversation_history", { request });
}

// Shows the main window. It starts hidden (visible: false in tauri.conf.json)
// so launches never flash a blank translucent shell; App calls this once the
// boot snapshot has been applied and the first real paint is imminent.
export function markAppWindowReady() {
  return invoke<void>("app_window_ready");
}

export function getLauncherAdapterPreference() {
  return invoke<string | null>("launcher_adapter_preference_get");
}

export function setLauncherAdapterPreference(adapterId: string) {
  return invoke<void>("launcher_adapter_preference_set", { adapterId });
}

// The OpenRouter API key lives in the backend's owner-only preferences file, not in
// webview localStorage, so the secret isn't readable at rest by injected scripts.
export function getOpenRouterKey() {
  return invoke<string>("openrouter_key_get");
}

export function setOpenRouterKey(key: string) {
  return invoke<void>("openrouter_key_set", { key });
}

// Proxies an OpenRouter chat-completion request through the Rust backend, which
// attaches the API key from the owner-only preferences file. The key is never sent
// from (or held for the request in) the renderer. Returns the upstream HTTP status
// and raw response body so the caller keeps its own parsing/retry logic.
export function openRouterChatCompletion(payload: unknown) {
  return invoke<{ status: number; body: string }>("openrouter_chat_completion", {
    payload,
  });
}

export function getPublishingAuthStatus() {
  return invoke<PublishingAuthStatus>("publishing_auth_status");
}

export function beginPublishingAuth() {
  return invoke<PublishingDeviceAuthorization>("publishing_auth_begin");
}

export function pollPublishingAuth(deviceCode: string) {
  return invoke<PublishingAuthPollResult>("publishing_auth_poll", { deviceCode });
}

export function disconnectPublishingAuth() {
  return invoke<PublishingAuthStatus>("publishing_auth_disconnect");
}

export function publishPublication(request: PublishPublicationRequest) {
  return invoke<PublicationBinding>("publishing_publish", { request });
}

export function syncPublication(request: SyncPublicationRequest) {
  return invoke<PublicationBinding>("publishing_sync", { request });
}

export function listPublications() {
  return invoke<PublicationBinding[]>("publishing_list");
}

export function listPublicationProposals(publicationId: string) {
  return invoke<PublicationProposal[]>("publishing_list_proposals", { publicationId });
}

export function resolvePublicationProposal(request: {
  publicationId: string;
  proposalCommentId: number;
  status: "accepted" | "declined";
  localNodeId?: string | null;
}) {
  return invoke<PublicationBinding>("publishing_resolve_proposal", {
    request,
  });
}

// The prompt library: reusable composer messages stored as markdown files, one
// file per prompt, in a global (~/.qmux/prompts/) or per-project
// (~/.qmux/projects/<basename>-<hash>/prompts/) scope. `projectDir` is the
// active pane's project directory (group dir, or base repo for worktrees);
// omit it when no project context exists and only the global scope is served.
export function listSavedPrompts(projectDir?: string | null) {
  return invoke<PromptLibrary>("prompt_library_list", { projectDir: projectDir ?? null });
}

// Creates or overwrites a saved prompt in `scope`. Passing a different
// previousScope/previousName renames or moves that prompt instead of leaving
// both files behind.
// `expectedModifiedMs` is the modifiedMs the caller last loaded for the prompt
// being updated/moved/deleted; the backend refuses the write if the file changed
// since (optimistic concurrency). Omit it for a brand-new prompt, whose write is
// create-only and has nothing to compare against.
export function saveSavedPrompt(
  scope: PromptScope,
  name: string,
  content: string,
  projectDir?: string | null,
  previous?: { scope: PromptScope; name: string } | null,
  expectedModifiedMs?: number | null,
) {
  return invoke<SavedPrompt>("prompt_library_save", {
    scope,
    name,
    content,
    projectDir: projectDir ?? null,
    previousScope: previous?.scope ?? null,
    previousName: previous?.name ?? null,
    expectedModifiedMs: expectedModifiedMs ?? null,
  });
}

export function deleteSavedPrompt(
  scope: PromptScope,
  name: string,
  projectDir?: string | null,
  expectedModifiedMs?: number | null,
) {
  return invoke<void>("prompt_library_delete", {
    scope,
    name,
    projectDir: projectDir ?? null,
    expectedModifiedMs: expectedModifiedMs ?? null,
  });
}

export function getActiveTab() {
  return invoke<string | null>("active_tab_get");
}

export function setActiveTab(tabId: string | null) {
  return invoke<void>("active_tab_set", { tabId });
}

export interface ShowHideShortcutSetting {
  accelerator: string | null;
  registered: boolean;
  error?: string | null;
  captureActive: boolean;
}

export type MenuBarStatusTone =
  | "active"
  | "pending"
  | "idle"
  | "attention"
  | "done"
  | "error";

export interface MenuBarTab {
  paneId: string;
  title: string;
  path?: string | null;
  statusTone: MenuBarStatusTone;
  statusLabel?: string | null;
  waitingOnPane: boolean;
  selected: boolean;
}

export interface MenuBarGroup {
  id: string;
  label: string;
  tabs: MenuBarTab[];
}

export interface MenuBarSnapshot {
  groups: MenuBarGroup[];
}

export interface MenuBarSelectPaneEvent {
  paneId: string;
}

export function getShowHideShortcut() {
  return invoke<ShowHideShortcutSetting>("show_hide_shortcut_get");
}

export function setShowHideShortcut(accelerator: string | null) {
  return invoke<ShowHideShortcutSetting>("show_hide_shortcut_set", { accelerator });
}

export function setShowHideShortcutCaptureActive(active: boolean) {
  return invoke<ShowHideShortcutSetting>("show_hide_shortcut_capture_set", { active });
}

export function getGlobalTaskLauncherHotkey() {
  return invoke<GlobalTaskLauncherSetting>("global_task_launcher_hotkey_get");
}

export function setGlobalTaskLauncherHotkey(hotkey: GlobalTaskLauncherHotkey | null) {
  return invoke<GlobalTaskLauncherSetting>("global_task_launcher_hotkey_set", { hotkey });
}

/** Opens the standalone quick-launch window (the ⌘K palette's path to it). */
export function openGlobalTaskLauncher() {
  return invoke<void>("global_task_launcher_open");
}

// Hides the launcher and returns focus to the app it was summoned from. Use on
// explicit dismissal (submit, Escape); a focus-loss dismissal should hide
// directly, since the OS has already moved focus to wherever the user clicked.
export function dismissGlobalTaskLauncher() {
  return invoke<void>("global_task_launcher_dismiss");
}

export function updateMenuBar(snapshot: MenuBarSnapshot) {
  return invoke<void>("menu_bar_update", { snapshot });
}

export function setMenuBarVisible(visible: boolean) {
  return invoke<void>("menu_bar_set_visible", { visible });
}

export function listenToMenuBarSelectPane(
  onSelectPane: (event: MenuBarSelectPaneEvent) => void,
): Promise<UnlistenFn> {
  return listen<MenuBarSelectPaneEvent>("menu-bar-select-pane", (event) =>
    onSelectPane(event.payload),
  );
}

/** Skills the qmux-managed Claude plugin can inject into launched Claude agents. */
export function listClaudeSkills() {
  return invoke<ClaudeSkill[]>("list_claude_skills");
}

export function listPanes() {
  return invoke<PaneInfo[]>("list_panes");
}

export function listGroups() {
  return invoke<GroupInfo[]>("list_groups");
}

export function ensureDefaultResearchWorkspace() {
  return invoke<GroupInfo>("ensure_default_research_workspace_command");
}

export function createResearchWorkspaceWithFolder() {
  return invoke<GroupInfo | null>("research_workspace_create_pick");
}

export function renameResearchWorkspace(workspaceId: string, name: string | null) {
  return invoke<GroupInfo>("research_workspace_rename", { workspaceId, name });
}

export function moveResearchWorkspaceWithFolder(workspaceId: string) {
  return invoke<GroupInfo | null>("research_workspace_move_pick", { workspaceId });
}

export function removeResearchWorkspace(workspaceId: string) {
  return invoke<string[]>("research_workspace_remove", { workspaceId });
}

export function revealResearchWorkspace(workspaceId: string) {
  return invoke<void>("research_workspace_reveal", { workspaceId });
}

export function pickGroupFolder() {
  return invoke<string | null>("group_pick_folder");
}

export interface GroupWithInitialPane {
  group: GroupInfo;
  pane: PaneInfo;
}

export function createGroupWithShell(
  dir: string,
  afterGroupId?: string | null,
  initialSize?: InitialPaneSize | null,
  remoteId?: string | null,
) {
  return invoke<GroupWithInitialPane>("group_create_with_shell", {
    dir,
    afterGroupId: afterGroupId ?? null,
    initialSize: initialSize ?? null,
    remoteId: remoteId ?? null,
  });
}

/** Creates a workspace, optionally bound to a machine declared under `remotes`
 * in qmux.config.json. The group snapshots that remote, so later config edits
 * never move a workspace whose worktrees already live on it. */
export function createGroup(options: {
  dir?: string | null;
  name?: string | null;
  afterGroupId?: string | null;
  remoteId?: string | null;
}) {
  return invoke<GroupInfo>("group_create", {
    request: {
      name: options.name ?? null,
      dir: options.dir ?? null,
      afterGroupId: options.afterGroupId ?? null,
      baseRepo: null,
      baseRef: null,
      remoteId: options.remoteId ?? null,
    },
  });
}

export function removeGroup(groupId: string) {
  return invoke<void>("group_remove", { groupId });
}

export function renameGroup(groupId: string, name: string | null) {
  return invoke<GroupInfo>("group_rename", { groupId, name });
}

export function reorderGroups(groupIds: string[]) {
  return invoke<GroupInfo[]>("group_reorder", { groupIds });
}

export function setGroupCollapsed(groupId: string, collapsed: boolean) {
  return invoke<GroupInfo>("group_set_collapsed", { groupId, collapsed });
}

export function pickGroupDirectory(groupId: string) {
  return invoke<GroupInfo | null>("group_pick_dir", { groupId });
}

export function listAgents() {
  return invoke<AgentInfo[]>("list_agents");
}

export function listTurns(agentId?: string | null) {
  return invoke<Turn[]>("list_turns", { agentId: agentId ?? null });
}

export function listHomeTurnHistory(
  agentId: string,
  before?: string | null,
  limit = 100,
) {
  return invoke<HomeTurnHistoryPage>("list_home_turn_history", {
    agentId,
    before: before ?? null,
    limit,
  });
}

export function listThreadGraphs() {
  return invoke<ThreadGraph[]>("list_thread_graphs");
}

export function getThreadGraph(threadId: string) {
  return invoke<ThreadGraph | null>("get_thread_graph", { threadId });
}

export function getConversationHistorySnapshot(snapshotId: string) {
  return invoke<ConversationHistorySnapshot | null>("get_conversation_history_snapshot", {
    snapshotId,
  });
}

export function listResearchTrees(includeArchived = false) {
  return invoke<ResearchTreeSummary[]>("list_research_trees", { includeArchived });
}

export function reorderResearchTrees(
  workspaceId: string,
  archived: boolean,
  treeIds: string[],
) {
  return invoke<void>("reorder_research_trees", { workspaceId, archived, treeIds });
}

export function listResearchFolders() {
  return invoke<ResearchFolderState>("list_research_folders");
}

/** Persists the grouping and returns the backend-normalized state. */
export function setResearchFolders(folders: ResearchFolderState) {
  return invoke<ResearchFolderState>("set_research_folders", { folders });
}

export function listResearchActivity() {
  return invoke<ResearchNode[]>("list_research_activity");
}

export function listRecentResearchQueries(
  limit = 50,
  before?: RecentResearchQueryCursor | null,
) {
  return invoke<RecentResearchQueryPage>("list_recent_research_queries", {
    limit,
    before: before ?? null,
  });
}

export function listRecentActivity(
  limit = 50,
  before?: RecentActivityCursor | null,
) {
  return invoke<RecentActivityPage>("list_recent_activity", {
    limit,
    before: before ?? null,
  });
}

export function appendJournalEntry(entry: JournalEntry) {
  return invoke<boolean>("journal_append", { entry });
}

export function restoreJournalEntry(entry: JournalEntry) {
  return invoke<boolean>("journal_restore", { entry });
}

export function updateJournalEntry(id: string, entry: JournalEntry) {
  return invoke<boolean>("journal_update", { id, entry });
}

export function deleteJournalEntry(id: string) {
  return invoke<boolean>("journal_remove", { id });
}

/** Fetches a tweet's raw syndication JSON through the backend (the webview
 * cannot reach X directly). `token` comes from syndicationToken(id). */
export function fetchJournalTweet(id: string, token: string) {
  return invoke<string>("journal_fetch_tweet", { id, token });
}

export function getNotificationLog() {
  return invoke<unknown>("notification_log_get");
}

export function markNotificationRead(id: string) {
  return invoke<unknown>("notification_log_mark_read", { id });
}

export function markAllNotificationsRead() {
  return invoke<unknown>("notification_log_mark_all_read");
}

export function clearNotificationLogEntry(id: string) {
  return invoke<unknown>("notification_log_clear", { id });
}

export function getResearchTree(treeId: string) {
  return invoke<ResearchTreeDetail>("get_research_tree", { treeId });
}

export function createResearchTree(request: {
  prompt: string;
  title?: string | null;
  adapter: string;
  model?: string | null;
  effort?: string | null;
  workspaceId: string;
}) {
  return invoke<ResearchTreeDetail>("create_research_tree", { request });
}

export function generateResearchAgentTitle(nodeId: string) {
  return invoke<string>("generate_research_agent_title", { nodeId });
}

export function createResearchDocument(request: {
  markdown: string;
  title?: string | null;
  workspaceId: string;
}) {
  return invoke<ResearchTreeDetail>("create_research_document", { request });
}

export function updateResearchDocument(request: {
  nodeId: string;
  markdown: string;
  title?: string | null;
  expectedResponseRevision: string;
  expectedTitle: string;
  expectedHighlightIds: string[];
}) {
  return invoke<UpdateResearchDocumentResult>("update_research_document", { request });
}

/** Reads a Markdown file selected through the native window drop API. The
 * backend enforces the extension, UTF-8 encoding, regular-file requirement,
 * and document byte cap before returning any content to the webview. */
export function readMarkdownDocumentFile(path: string) {
  return invoke<string>("read_markdown_document_file", { path });
}

/** Reads a pasted image referenced by a transcript "[Image: source: <path>]"
 * marker and returns it as a data: URL for direct use in an <img> tag. The
 * backend confines reads to the home or platform temporary directory and
 * enforces the raster extension allowlist, regular-file requirement, and byte
 * cap. */
export function readTranscriptImage(path: string) {
  return invoke<string>("read_transcript_image", { path });
}

/** Persists a base64-encoded image pasted into a composer/queue into the image
 * cache and returns its absolute path, for referencing in the prompt as
 * "[Image: <path>]". `extension` is the raster format (png/jpg/jpeg/gif/webp/bmp);
 * the backend enforces the allowlist and byte cap. */
export function savePastedImage(dataBase64: string, extension: string) {
  return invoke<string>("save_pasted_image", { dataBase64, extension });
}

/** Copies a terminal agent pane's conversation into a Research workspace as
 * a read-only conversation tree. The terminal is untouched — repeating the
 * export creates another independent tree. */
export function exportPaneToResearch(request: {
  paneId: string;
  workspaceId: string;
  title?: string | null;
}) {
  return invoke<ResearchTreeDetail>("export_pane_to_research", { request });
}

export function getResearchNodeContent(nodeId: string) {
  return invoke<ResearchNodeContent>("get_research_node_content", { nodeId });
}

export function forkResearchNode(
  parentNodeId: string,
  prompt: string,
  publicationProposal?: {
    publicationId: string;
    commentId: number;
  } | null,
  queryAnchor?: ResearchHighlightAnchor | null,
  inline = false,
) {
  return invoke<ResearchNode>("fork_research_node", {
    parentNodeId,
    prompt,
    publicationProposal: publicationProposal ?? null,
    queryAnchor: queryAnchor ?? null,
    inline,
  });
}

/** Relaunches a failed (or cancelled) run in place: the node keeps its id and
 * launch inputs, resets to queued, and goes back through the ordinary launch
 * machinery. Returns the refreshed tree detail. */
export function retryResearchNode(nodeId: string) {
  return invoke<ResearchTreeDetail>("retry_research_node", { nodeId });
}

export function cancelResearchNode(nodeId: string) {
  return invoke<ResearchNode>("cancel_research_node", { nodeId });
}

export function renameResearchTree(treeId: string, title: string) {
  return invoke<ResearchTree>("rename_research_tree", { treeId, title });
}

export function renameResearchNode(nodeId: string, title: string) {
  return invoke<ResearchNode>("rename_research_node", { nodeId, title });
}

export function createResearchHighlight(
  nodeId: string,
  anchor: ResearchHighlightAnchor,
) {
  return invoke<ResearchHighlight>("create_research_highlight", {
    nodeId,
    anchor,
  });
}

export function removeResearchHighlight(nodeId: string, highlightId: string) {
  return invoke<ResearchHighlight>("remove_research_highlight", {
    nodeId,
    highlightId,
  });
}

export function removeResearchHighlights(nodeId: string, highlightIds: string[]) {
  return invoke<ResearchHighlight[]>("remove_research_highlights", {
    nodeId,
    highlightIds,
  });
}

export function markResearchTreeViewed(treeId: string) {
  return invoke<ResearchTree>("mark_research_tree_viewed", { treeId });
}

export function archiveResearchTree(treeId: string) {
  return invoke<ResearchTree>("archive_research_tree", { treeId });
}

export function restoreResearchTree(treeId: string) {
  return invoke<ResearchTree>("restore_research_tree", { treeId });
}

export function removeResearchTree(treeId: string) {
  return invoke<void>("remove_research_tree", { treeId });
}

export function removeResearchBranch(nodeId: string) {
  return invoke<ResearchBranchRemoval>("remove_research_branch", { nodeId });
}

export function listAgentTurnQueue(agentId: string) {
  return invoke<QueuedTurn[]>("list_agent_turn_queue", { agentId });
}

export function listGlobalDrafts() {
  return invoke<GlobalDraft[]>("list_global_drafts");
}

export function createGlobalDraft(text: string) {
  return invoke<GlobalDraft>("create_global_draft", { text });
}

export function updateGlobalDraft(draftId: string, text: string) {
  return invoke<GlobalDraft>("update_global_draft", { draftId, text });
}

export function deleteGlobalDraft(draftId: string) {
  return invoke<GlobalDraft[]>("delete_global_draft", { draftId });
}

export interface AssignGlobalDraftResult {
  sent: boolean;
  drafts: GlobalDraft[];
  queuedTurns: QueuedTurn[];
}

/** Hands a draft to an agent atomically: claim, then send-or-queue, with the
 * claim rolled back if the submit fails. */
export function assignGlobalDraft(draftId: string, agentId: string) {
  return invoke<AssignGlobalDraftResult>("assign_global_draft", {
    request: { draftId, agentId },
  });
}

/** Toggles the pause-after-send flag on one queued turn. `expectedId` is the
 * turn's stable id; the backend rejects the change if the turn at `index` is no
 * longer that turn (a duplicate-text turn shifted into place). */
export function setQueuedTurnPause(
  agentId: string,
  index: number,
  pauseAfter: boolean,
  expectedData: string,
  expectedId?: string | null,
) {
  return invoke<QueuedTurn[]>("agent_set_queued_turn_pause", {
    agentId,
    index,
    pauseAfter,
    expectedData,
    expectedId: expectedId ?? null,
  });
}

/** Clears an agent's paused state, draining the next queued turn if it is idle. */
export function unpauseAgent(agentId: string) {
  return invoke<SendNextQueuedAgentTurnResult>("agent_unpause", { agentId });
}

export function listAgentTranscripts(agentId: string) {
  return invoke<TranscriptOption[]>("list_agent_transcripts", { agentId });
}

export function setAgentTranscript(agentId: string, path: string | null) {
  return invoke<AgentInfo>("set_agent_transcript", { agentId, path });
}

export async function spawnShell(
  initialSize?: InitialPaneSize | null,
  sourcePaneId?: string | null,
  groupId?: string | null,
  remoteId?: string | null,
) {
  const started = performance.now();
  const pane = await invoke<PaneInfo>("spawn_shell", {
    initialSize: initialSize ?? null,
    sourcePaneId: sourcePaneId ?? null,
    groupId: groupId ?? null,
    remoteId: remoteId ?? null,
  });
  if (pane.remoteSession) {
    trackRemoteStartup(pane.id, started);
    recordRemoteStartup(pane.id, "reserved");
    const observed = reconcileRemoteReservation(pane).remoteConnection;
    if (observed?.state === "connected") recordRemoteStartup(pane.id, "ready");
    if (observed?.state === "failed") forgetRemoteStartup(pane.id);
  }
  return pane;
}

export function openPaneWorktree(
  paneId: string,
  worktreeName: string,
  initialSize?: InitialPaneSize | null,
) {
  return invoke<PaneInfo>("open_pane_worktree", {
    paneId,
    worktreeName,
    initialSize: initialSize ?? null,
  });
}

export function suggestPaneWorktreeName(paneId: string) {
  return invoke<string>("suggest_pane_worktree_name", { paneId });
}

export function getUseLoginShell() {
  return invoke<boolean>("use_login_shell_get");
}

export function setUseLoginShell(enabled: boolean) {
  return invoke<void>("use_login_shell_set", { enabled });
}

export function getWorktreeLocation() {
  return invoke<WorktreeLocation>("worktree_location_get");
}

export function setWorktreeLocation(location: WorktreeLocation) {
  return invoke<void>("worktree_location_set", { location });
}

export function getResearchLaunchInstruction() {
  return invoke<string>("research_launch_instruction_get");
}

export function setResearchLaunchInstruction(instruction: string) {
  return invoke<void>("research_launch_instruction_set", { instruction });
}

export function getResearchSdkHarness() {
  return invoke<boolean>("research_sdk_harness_get");
}

export function setResearchSdkHarness(enabled: boolean) {
  return invoke<void>("research_sdk_harness_set", { enabled });
}

export function spawnAgent(request: SpawnAgentRequest) {
  return invoke<PaneInfo>("agent_spawn", { request });
}

// Forks the session in `paneId` into a new tab immediately after it and resumes
// the session. `prompt` is submitted as the fork's launch message.
//
// `anchor` forks from a chosen message instead of the session head: the backend
// synthesizes a transcript ending just before it and resumes that. The anchor is
// resolved against the pane's own transcript, so it can only ever address that
// session's messages.
export function forkAgent(
  paneId: string,
  options?: {
    useWorktree?: boolean;
    worktreeName?: string;
    prompt?: string;
    anchor?: MessageAnchor;
  },
) {
  return invoke<PaneInfo>("agent_fork", {
    paneId,
    useWorktree: options?.useWorktree ?? false,
    worktreeName: options?.worktreeName,
    prompt: options?.prompt,
    anchor: options?.anchor,
  });
}

export function submitPaneInput(paneId: string, data: string) {
  return invoke<void>("pane_write", { paneId, data, paste: true, submit: true });
}

export function submitAgentTurn(agentId: string, data: string, mode: SubmitAgentTurnMode = "auto") {
  return invoke<SubmitAgentTurnResult>("agent_submit_turn", {
    request: { agentId, data, mode },
  });
}

export function queueWaitAgentTurn(
  agentId: string,
  data: string,
  waitForAgentId: string,
  waitForPaneId?: string | null,
  waitForLabel?: string | null,
) {
  return invoke<SubmitAgentTurnResult>("agent_queue_wait_turn", {
    request: {
      agentId,
      data,
      waitForAgentId,
      waitForPaneId: waitForPaneId ?? null,
      waitForLabel: waitForLabel ?? null,
    },
  });
}

// Queues a turn that, when reached, is delivered to a new pane (a fork of this
// session or a fresh session in the same directory) instead of this agent's own
// composer.
export function queueDeliveryAgentTurn(
  agentId: string,
  data: string,
  delivery: QueuedTurnDelivery,
) {
  return invoke<SubmitAgentTurnResult>("agent_queue_delivery_turn", {
    request: { agentId, data, delivery },
  });
}

export function removeQueuedAgentTurn(
  agentId: string,
  index: number,
  expectedData: string,
  expectedId?: string | null,
) {
  return invoke<RemoveQueuedAgentTurnResult>("agent_remove_queued_turn", {
    request: { agentId, index, expectedData, expectedId: expectedId ?? null },
  });
}

export function reorderQueuedAgentTurn(
  agentId: string,
  fromIndex: number,
  toIndex: number,
  expectedData: string,
  expectedId?: string | null,
) {
  return invoke<ReorderQueuedAgentTurnResult>("agent_reorder_queued_turn", {
    request: { agentId, fromIndex, toIndex, expectedData, expectedId: expectedId ?? null },
  });
}

export function sendNextQueuedAgentTurn(agentId: string) {
  return invoke<SendNextQueuedAgentTurnResult>("agent_send_next_queued_turn", { agentId });
}

export type AgentDebugInputKind = "textOnly" | "returnOnly" | "textAndReturn";

/** Exercises the same adapter-specific PTY payload/submit options used by a
 * queued turn, without touching lifecycle or prompt-correlation state. */
export function sendAgentDebugInput(agentId: string, kind: AgentDebugInputKind) {
  return invoke<void>("agent_debug_input", { agentId, kind });
}

/** Transient queue-to-PTY state for the opt-in delivery Debug panel. */
export function getAgentDeliveryDebug(agentId: string) {
  return invoke<AgentDeliveryDebugInfo>("agent_delivery_debug", { agentId });
}

/** Marks/clears that the user is actively typing for an agent, so the backend holds
 *  off auto-draining its queue. Clearing drains a held turn if the agent is idle. */
export function setAgentTyping(agentId: string, typing: boolean) {
  return invoke<SendNextQueuedAgentTurnResult>("agent_set_typing", { agentId, typing });
}

/** Opens an http(s)/mailto URL in the user's default external browser/mail client. */
export function openExternalUrl(url: string) {
  return invoke<void>("open_external_url", { url });
}

/** Opens the source file behind a protected qmux preview as a validated file:// URL. */
export function browserOpenPreviewExternal(url: string) {
  return invoke<void>("browser_open_preview_external", { url });
}

export type BrowserOpenLocalPathResult = {
  disposition: "preview" | "revealed";
  url: string | null;
  sandbox: boolean;
};

/** Safely open an absolute local path: preview known renderable files in the
 * sandboxed overlay and reveal unknown/binary formats in the OS file manager. */
export function browserOpenLocalPath(paneId: string, path: string, artifactId?: string) {
  return invoke<BrowserOpenLocalPathResult>("browser_open_local_path", {
    paneId,
    path,
    artifactId,
  });
}

/** Safely open a path recognized by the native terminal. Relative paths are
 * resolved against that pane's backend-recorded live cwd. */
export function browserOpenTerminalPath(paneId: string, path: string) {
  return invoke<BrowserOpenLocalPathResult>("browser_open_terminal_path", {
    paneId,
    path,
  });
}

/** Reveal a root-confined local path without opening or executing it. */
export function browserRevealLocalPath(paneId: string, path: string) {
  return invoke<void>("browser_reveal_local_path", { paneId, path });
}

/** Deliberately hand a root-confined local path to its OS default app. */
export function browserOpenLocalPathExternal(paneId: string, path: string) {
  return invoke<void>("browser_open_local_path_external", { paneId, path });
}

/** Resolve a Codex inline-visualization basename within the pane's own session
 * directory and open its fragment in the sandboxed browser overlay. */
export function browserOpenCodexInlineVisualization(paneId: string, file: string) {
  return invoke<{ url: string; sandbox: boolean }>(
    "browser_open_codex_inline_visualization",
    { paneId, file },
  );
}

/** Open an absolute fragment path from the current Codex visualization
 * content-reference contract after backend root confinement. */
export function browserOpenCodexVisualizationReference(paneId: string, path: string) {
  return invoke<{ url: string; sandbox: boolean }>(
    "browser_open_codex_visualization_reference",
    { paneId, path },
  );
}

export function artifactList() {
  return invoke<ArtifactInfo[]>("artifact_list");
}

/** Removes an artifact-tray entry; returns it so the tray's undo can restore it. */
export function artifactRemove(artifactId: string) {
  return invoke<ArtifactInfo>("artifact_remove", { artifactId });
}

export function artifactRestore(artifact: ArtifactInfo) {
  return invoke<void>("artifact_restore", { artifact });
}

/** Opens an artifact outside qmux: URLs in the default browser, files with the
 * OS default app for the file type. */
export function artifactOpenExternal(artifactId: string) {
  return invoke<void>("artifact_open_external", { artifactId });
}

/** Reveals a file artifact in the OS file manager, selecting the file. */
export function artifactReveal(artifactId: string) {
  return invoke<void>("artifact_reveal", { artifactId });
}

/** Token-scoped file-server URL for a file artifact (thumbnails/previews), or
 * null when the source pane is gone or the file left the pane's roots. */
export function artifactFileUrl(artifactId: string) {
  return invoke<string | null>("artifact_file_url", { artifactId });
}

export type HumanBrowserSnapshot = {
  ownerId: string;
  url: string;
  canGoBack: boolean;
  canGoForward: boolean;
};

export type HumanBrowserEvent = {
  ownerId: string;
  kind: "navigation" | "title" | "newWindow";
  url: string | null;
  title: string | null;
  loading: boolean | null;
};

export type HumanBrowserSync = {
  ownerId: string;
  url: string;
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
  navigationRevision: number;
};

// Visibility belongs to one app-global native surface, so sync revisions order
// all geometry/show requests. The backend additionally tracks lifecycle order
// per owner so another pane's update cannot accidentally suppress a destroy.
let humanBrowserSurfaceRevision = 0;
let humanBrowserGeneration: Promise<number> | null = null;
const humanBrowserLifecycleQueue = new HumanBrowserLifecycleQueue();

function getHumanBrowserGeneration() {
  humanBrowserGeneration ??= invoke<number>("human_browser_generation");
  return humanBrowserGeneration;
}

export function syncHumanBrowser(request: HumanBrowserSync) {
  return humanBrowserLifecycleQueue.enqueue(() =>
    retryHumanBrowserLifecycle(async () => {
      humanBrowserSurfaceRevision += 1;
      const revision = humanBrowserSurfaceRevision;
      const generation = await getHumanBrowserGeneration();
      return invoke<HumanBrowserSnapshot | null>("human_browser_sync", {
        request: { ...request, generation, revision },
      });
    }),
  );
}

export function destroyHumanBrowser(ownerId: string) {
  return humanBrowserLifecycleQueue.enqueue(() =>
    retryHumanBrowserLifecycle(async () => {
      humanBrowserSurfaceRevision += 1;
      const revision = humanBrowserSurfaceRevision;
      const generation = await getHumanBrowserGeneration();
      return invoke<void>("human_browser_destroy", {
        request: { ownerId, generation, revision },
      });
    }),
  );
}

/** Collapse every native child. Returns how many views were hidden. */
export function hideAllHumanBrowsers() {
  return humanBrowserLifecycleQueue.enqueue(() =>
    retryHumanBrowserLifecycle(async () => {
      humanBrowserSurfaceRevision += 1;
      const revision = humanBrowserSurfaceRevision;
      const generation = await getHumanBrowserGeneration();
      return invoke<number>("human_browser_hide_all", {
        request: { generation, revision },
      });
    }),
  );
}

export async function getHumanBrowserSnapshot(ownerId: string) {
  const generation = await getHumanBrowserGeneration();
  return invoke<HumanBrowserSnapshot | null>("human_browser_snapshot", {
    request: { ownerId, generation },
  });
}

export function reloadHumanBrowser(ownerId: string) {
  return humanBrowserLifecycleQueue.enqueue(async () => {
    const generation = await getHumanBrowserGeneration();
    return invoke<void>("human_browser_reload", {
      request: { ownerId, generation },
    });
  });
}

export function navigateHumanBrowserHistory(ownerId: string, direction: "back" | "forward") {
  return humanBrowserLifecycleQueue.enqueue(async () => {
    const generation = await getHumanBrowserGeneration();
    return invoke<void>("human_browser_navigate_history", {
      request: { ownerId, generation },
      direction,
    });
  });
}

export function listenToHumanBrowserEvents(
  onEvent: (event: HumanBrowserEvent) => void,
): Promise<UnlistenFn> {
  return listen<HumanBrowserEvent>("human-browser-event", (event) => onEvent(event.payload));
}

export type BrowserAutomationSnapshot = {
  available: boolean;
  tabId: number | null;
  url: string | null;
  title: string | null;
  canGoBack: boolean;
  canGoForward: boolean;
  imageDataUrl: string | null;
  width: number;
  height: number;
  error: string | null;
};

export function getBrowserAutomationSnapshot(
  paneId: string,
  width: number,
  height: number,
  scaleFactor: number,
) {
  return invoke<BrowserAutomationSnapshot>("browser_automation_snapshot", {
    paneId,
    width,
    height,
    scaleFactor,
  });
}

/** One mirrored frame pushed by Chromium's screencast, ready for an <img>. */
export type BrowserScreencastFrame = {
  paneId: string;
  tabId: number;
  url: string;
  title: string;
  width: number;
  height: number;
  imageDataUrl: string;
};

/**
 * Start (or reconfigure) the pane's screencast and report the mirrored tab.
 * The backend only touches Chromium when the tab, size, or scale changed, so
 * this doubles as the overlay's metadata heartbeat.
 */
export function startBrowserScreencast(
  paneId: string,
  width: number,
  height: number,
  scaleFactor: number,
) {
  return invoke<BrowserAutomationSnapshot>("browser_automation_start_screencast", {
    paneId,
    width,
    height,
    scaleFactor,
  });
}

export function stopBrowserScreencast(paneId: string) {
  return invoke<void>("browser_automation_stop_screencast", { paneId });
}

export function listenToBrowserScreencastFrames(
  onFrame: (frame: BrowserScreencastFrame) => void,
): Promise<UnlistenFn> {
  return listen<BrowserScreencastFrame>("browser-screencast-frame", (event) =>
    onFrame(event.payload),
  );
}

export function navigateBrowserAutomation(paneId: string, url: string) {
  return invoke<void>("browser_automation_navigate", { paneId, url });
}

export function reloadBrowserAutomation(paneId: string) {
  return invoke<void>("browser_automation_reload", { paneId });
}

export function navigateBrowserAutomationHistory(
  paneId: string,
  direction: "back" | "forward",
) {
  return invoke<void>("browser_automation_navigate_history", { paneId, direction });
}

export function sendBrowserAutomationMouse(
  paneId: string,
  kind: "move" | "down" | "up" | "click" | "scroll",
  x: number,
  y: number,
  deltaX?: number,
  deltaY?: number,
  button?: "left" | "middle" | "right" | "none",
  buttons?: number,
  modifiers?: number,
) {
  return invoke<void>("browser_automation_mouse", {
    paneId,
    kind,
    x,
    y,
    deltaX: deltaX ?? null,
    deltaY: deltaY ?? null,
    button: button ?? null,
    buttons: buttons ?? null,
    modifiers: modifiers ?? null,
  });
}

export function insertBrowserAutomationText(paneId: string, text: string) {
  return invoke<void>("browser_automation_insert_text", { paneId, text });
}

export function sendBrowserAutomationKey(
  paneId: string,
  key: string,
  code: string,
  windowsVirtualKeyCode: number,
  modifiers = 0,
) {
  return invoke<void>("browser_automation_key", {
    paneId,
    key,
    code,
    windowsVirtualKeyCode,
    modifiers,
  });
}

// Atomically moves a queued turn from one agent to another. The backend removes
// from the source and hands it to the target in one call, rolling back on failure,
// so the turn can never end up in both queues or be lost.
export function moveQueuedAgentTurn(
  fromAgentId: string,
  toAgentId: string,
  index: number,
  expectedData: string,
  expectedId?: string | null,
) {
  return invoke<MoveQueuedAgentTurnResult>("agent_move_queued_turn", {
    request: { fromAgentId, toAgentId, index, expectedData, expectedId: expectedId ?? null },
  });
}

export function setAgentDraft(agentId: string, draft: string) {
  return invoke<void>("agent_set_draft", { agentId, draft });
}

export function getAgentDraft(agentId: string) {
  return invoke<string | null>("agent_get_draft", { agentId });
}

export function getInterfaceDraft(key: string) {
  return invoke<string | null>("interface_draft_get", { key });
}

export function setInterfaceDraft(key: string, value: string | null) {
  return invoke<void>("interface_draft_set", { key, value });
}

export function acknowledgeAgent(agentId: string, includeFailed = false) {
  return invoke<AgentInfo>("agent_acknowledge", { agentId, includeFailed });
}

export function clearAgentWorkingStatus(agentId: string) {
  return invoke<AgentInfo>("agent_clear_working_status", { agentId });
}

/**
 * Tells the backend the listener for this pane is live, flushing any PTY output
 * buffered before the webview subscribed (e.g. the cold-start prompt). Must be
 * called only after listenToEvents has resolved.
 */
export function attachPane(paneId: string) {
  return invoke<void>("pane_attach", { paneId });
}

export interface NativeTerminalLayout {
  paneId: string;
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
  acceptsPointerInput: boolean;
  /**
   * Whether a pointer gesture may optimistically grant this pane the keyboard
   * before React confirms the desired owner (native click-to-focus). False when the
   * keyboard denial is hard policy — read-only research panes, blocked input —
   * rather than a transient focus state like an active web editable.
   */
  acceptsKeyboardClaim: boolean;
  deferGeometry: boolean;
}

// Seed from wall time so a webview/module reload cannot restart revisions below
// the native host's last applied value. Multiplying by 1,000 leaves room for
// bursts within one millisecond while remaining below Number.MAX_SAFE_INTEGER.
let nativeTerminalLayoutRevision = Date.now() * 1_000;

/**
 * Publishes a pane's native geometry. Layout updates are revisioned so an
 * older fire-and-forget invoke (common around split close / right-pane
 * toggles) can never overwrite a newer frame that already reached AppKit.
 */
export function setNativeTerminalLayout(layout: NativeTerminalLayout) {
  nativeTerminalLayoutRevision = Math.max(
    nativeTerminalLayoutRevision + 1,
    Date.now() * 1_000,
  );
  return invoke<void>("native_terminal_set_layout", {
    layout: {
      ...layout,
      revision: nativeTerminalLayoutRevision,
    },
  });
}

// Seed from wall time so a webview/module reload cannot restart revisions below
// the native host's last applied value. Multiplying by 1,000 leaves room for
// bursts within one millisecond while remaining below Number.MAX_SAFE_INTEGER.
let nativeTerminalKeyboardOwnerRevision = Date.now() * 1_000;

/**
 * Publishes the frontend's complete desired native keyboard owner. Ownership
 * updates are revisioned independently from pane geometry so an older invoke
 * can never overtake a newer activation/release and reclaim the keyboard.
 */
export function setNativeTerminalKeyboardOwner(paneId: string | null) {
  nativeTerminalKeyboardOwnerRevision = Math.max(
    nativeTerminalKeyboardOwnerRevision + 1,
    Date.now() * 1_000,
  );
  return invoke<void>("native_terminal_set_keyboard_owner", {
    update: {
      paneId,
      revision: nativeTerminalKeyboardOwnerRevision,
    },
  });
}

/**
 * Tells the native key monitor whether the currently rendered browser overlay
 * owns Escape. AppKit sees key events before either Ghostty or a child browser
 * document, so this closes the focus-routing gap between those surfaces and
 * the outer React document's Escape dispatcher. Serialize transitions so a
 * rapid open/close cannot leave the native claim stuck on if invokes complete
 * out of order; a failed update must not poison the next transition.
 */
let nativeTerminalBrowserOverlayUpdate: Promise<void> = Promise.resolve();
export function setNativeTerminalBrowserOverlayOpen(active: boolean) {
  nativeTerminalBrowserOverlayUpdate = nativeTerminalBrowserOverlayUpdate
    .catch(() => undefined)
    .then(() => invoke<void>("native_terminal_set_browser_overlay_open", { active }));
  return nativeTerminalBrowserOverlayUpdate;
}

/** Enables viewport/content events only while a visible pane has annotations. */
export function setNativeTerminalAnnotationMonitoring(paneId: string, enabled: boolean) {
  return invoke<void>("native_terminal_set_annotation_monitoring", { paneId, enabled });
}

export interface NativeWebOverlayRegion {
  regionId: string;
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
}

/**
 * Registers a DOM rectangle whose pointer events stay with WKWebView even
 * though it overlaps a native terminal surface — for small controls that float
 * over the terminal. Unlike claimNativeTerminalPointerForWebDrag, the rest of
 * the terminal keeps receiving clicks. `visible: false` removes the region.
 */
export function setNativeTerminalWebOverlayRegion(region: NativeWebOverlayRegion) {
  return invoke<void>("native_terminal_set_web_overlay_region", { region });
}

/**
 * Reports whether DOM focus sits inside a cross-document iframe (the browser
 * overlay's page). Keys typed there are delivered to the framed document only,
 * so native routing must claim recognized app shortcuts while it is active.
 */
export function setNativeTerminalIframeShortcutFallback(active: boolean) {
  return invoke<void>("native_terminal_set_iframe_shortcut_fallback", { active });
}

let nativeTerminalWebPointerClaims = 0;
let nativeTerminalWebPointerUpdate: Promise<void> = Promise.resolve();

function queueNativeTerminalWebPointerClaim(claimed: boolean) {
  // Preserve start/end order even when a very short drag releases before the
  // first invoke has completed. The claim is global native state — a dropped
  // update (a release especially) leaves every terminal mouse-dead until some
  // later claim cycle happens to rewrite it — so transient bridge failures
  // are retried. Retries run inside the serialized chain, so a newer update
  // can never be overtaken by an older retry; errors are still absorbed at
  // the end so a persistent failure cannot poison later ownership updates.
  nativeTerminalWebPointerUpdate = nativeTerminalWebPointerUpdate
    .catch(() => undefined)
    .then(async () => {
      for (let attempt = 0; ; attempt += 1) {
        try {
          await invoke<void>("native_terminal_set_web_pointer_claimed", { claimed });
          return;
        } catch (err) {
          if (attempt >= 2) {
            throw err;
          }
          await new Promise((resolve) => setTimeout(resolve, 50 * (attempt + 1)));
        }
      }
    })
    .catch(() => undefined);
}

/**
 * Temporarily gives WKWebView every pointer event, including events whose
 * coordinates overlap a native terminal surface. Used for mid-gesture drag
 * controls and for sticky overlays (sidebar menus) that open over the terminal.
 * Claims are reference-counted so independently mounted claimants cannot
 * release each other's capture. Call the returned function to release.
 */
export function claimNativeTerminalPointerForWebDrag(): () => void {
  nativeTerminalWebPointerClaims += 1;
  if (nativeTerminalWebPointerClaims === 1) {
    queueNativeTerminalWebPointerClaim(true);
  }

  let released = false;
  return () => {
    if (released) {
      return;
    }
    released = true;
    nativeTerminalWebPointerClaims = Math.max(0, nativeTerminalWebPointerClaims - 1);
    if (nativeTerminalWebPointerClaims === 0) {
      queueNativeTerminalWebPointerClaim(false);
    }
  };
}

/** Positions the opaque native backstop under the terminal stage, so transient
 * gaps while pane surfaces chase their DOM rects show terminal-colored pixels
 * instead of the window's vibrancy material. */
export function setNativeTerminalStageBackstop(rect: {
  x: number;
  y: number;
  width: number;
  height: number;
}) {
  return invoke<void>("native_terminal_set_stage_backstop", rect);
}

export function focusNativeTerminal(paneId: string) {
  return invoke<void>("native_terminal_focus", { paneId });
}

export interface NativeTerminalSettings {
  paneId: string;
  fontSize: number;
  fontFamily: string;
  letterSpacing: number;
  lineHeight: number;
  cursorBlink: boolean;
  cursorStyle: "block" | "underline" | "bar";
  scrollbackRows: number;
  scrollOnUserInput: boolean;
  scrollSensitivity: number;
  copyOnSelect: boolean;
  selectionClearOnCopy: boolean;
  themeName: string;
}

// Seed from wall time so a webview/module reload cannot restart revisions below
// the native host's last applied value. Theme previews can publish settings much
// faster than their Tauri invokes complete, so every snapshot needs ordering at
// the native boundary rather than relying on promise completion order.
let nativeTerminalSettingsRevision = Date.now() * 1_000;

function nextNativeTerminalSettingsRevision(): number {
  nativeTerminalSettingsRevision = Math.max(
    nativeTerminalSettingsRevision + 1,
    Date.now() * 1_000,
  );
  return nativeTerminalSettingsRevision;
}

export interface NativeTerminalTheme {
  name: string;
  /** Bare RRGGBB hex, no leading '#'. */
  background: string;
  /** Bare RRGGBB hex, no leading '#'. */
  foreground: string;
  isDark: boolean;
  /** The 16 ANSI palette colors; entries are empty when a scheme omits them. */
  palette: string[];
}

export function performNativeTerminalAction(paneId: string, action: string) {
  return invoke<void>("native_terminal_action", { paneId, action });
}

export function pasteApprovedNativeTerminalText(paneId: string, text: string) {
  return invoke<void>("native_terminal_paste_approved_text", { paneId, text });
}

export function updateNativeTerminalSettings(settings: NativeTerminalSettings) {
  return invoke<void>("native_terminal_update_settings", {
    settings: {
      ...settings,
      revision: nextNativeTerminalSettingsRevision(),
    },
  });
}

/**
 * Hands the native host a pane-independent settings snapshot to cache, so a
 * pane created later builds its Ghostty surface at creation time instead of
 * waiting for its own mount-time settings round-trip. Called at startup and
 * whenever terminal settings change.
 */
export function seedNativeTerminalSettings(settings: Omit<NativeTerminalSettings, "paneId">) {
  return invoke<void>("native_terminal_seed_settings", {
    settings: {
      ...settings,
      revision: nextNativeTerminalSettingsRevision(),
    },
  });
}

/**
 * The terminal theme catalog: the qmux default first, then every Ghostty
 * color scheme bundled with libghostty-spm. Empty on platforms without
 * native terminals.
 */
export async function listNativeTerminalThemes(): Promise<NativeTerminalTheme[]> {
  const catalog = await invoke<string>("native_terminal_theme_catalog");
  return JSON.parse(catalog) as NativeTerminalTheme[];
}

/**
 * Plain-text snapshot of a native terminal's visible viewport (no scrollback,
 * no SGR colors). Used by the expanded-transcript PiP preview.
 */
export function readNativeTerminalViewportText(paneId: string) {
  return invoke<string>("native_terminal_read_viewport_text", { paneId });
}

export interface NativeTerminalAnnotationSelectionSnapshot {
  selectedText: string;
  viewportCellStart: number;
  viewportCellLength: number;
  selectionStartXPoints: number;
  selectionBaselineYPoints: number;
  scrollbar: {
    totalRows: number;
    offsetRows: number;
    visibleRows: number;
  };
  scrollbarIsInitialized: boolean;
  columns: number;
  rows: number;
  cellWidthPoints: number;
  cellHeightPoints: number;
  gridOriginXPoints: number;
  gridOriginYPoints: number;
  backingScaleFactor: number;
  viewportRevision: number;
  contentGeneration: number;
  viewportFullyContained: boolean;
}

export type NativeTerminalAnnotationViewportSnapshot = Omit<
  NativeTerminalAnnotationSelectionSnapshot,
  | "selectedText"
  | "viewportCellStart"
  | "viewportCellLength"
  | "selectionStartXPoints"
  | "selectionBaselineYPoints"
  | "viewportFullyContained"
>;

/**
 * Current native selection and geometry. A false `viewportFullyContained`
 * allows quote capture but must never be used to paint a cell anchor.
 */
export async function readNativeTerminalAnnotationSelection(
  paneId: string,
): Promise<NativeTerminalAnnotationSelectionSnapshot> {
  const snapshot = await invoke<string>("native_terminal_annotation_selection_snapshot", {
    paneId,
  });
  return JSON.parse(snapshot) as NativeTerminalAnnotationSelectionSnapshot;
}

export function paneActivity(paneId: string) {
  return invoke<PaneActivity>("pane_activity", { paneId });
}

export function killPane(paneId: string) {
  return invoke<void>("pane_kill", { paneId });
}

// Records the focused pane so the backend can pick a group's most-recently-active
// shell pane when resolving a spawn cwd. Best-effort; failures are ignored.
export function activatePane(paneId: string) {
  return invoke<void>("pane_activate", { paneId });
}

export function restoreLastClosedPane() {
  return invoke<PaneInfo | null>("pane_restore_last_closed");
}

export function renamePane(paneId: string, title: string) {
  return invoke<PaneInfo>("pane_rename", { paneId, title });
}

export function generateFoundationTabTitle(message: string) {
  return invoke<string>("generate_foundation_tab_title", { message });
}

/** Atomically sets the flat sidebar tab order in one call. */
export function setPaneLayout(items: PaneLayoutItem[]) {
  return invoke<PaneInfo[]>("pane_set_layout", { items });
}

/** Moves `paneId` into `targetGroupId`, applying `items` as the resulting flat tab
 * order in the same backend mutation. Shell tabs only —
 * the backend rejects agent tabs, whose worktrees are bound to their group. */
export function movePaneToGroup(
  paneId: string,
  targetGroupId: string,
  items: PaneLayoutItem[],
) {
  return invoke<PaneInfo[]>("pane_move_to_group", { paneId, targetGroupId, items });
}

/** Moves `paneId` immediately after `siblingPaneId` in the flat sidebar order. */
export function placePaneAfter(paneId: string, siblingPaneId: string) {
  return invoke<PaneInfo[]>("pane_place_after", { paneId, siblingPaneId });
}

export function getPaneSplits() {
  return invoke<PaneSplitInfo[]>("pane_splits_get");
}

export function setPaneSplits(splits: PaneSplitInfo[]) {
  return invoke<PaneSplitInfo[]>("pane_splits_set", { splits });
}

export function worktreeStatus(agentId: string) {
  return invoke<WorktreeStatus>("worktree_status", { agentId });
}

export function closeWorktreePane(agentId: string, deleteWorktree: boolean) {
  return invoke<void>("worktree_close_pane", { agentId, deleteWorktree });
}

export function confirmAppExit() {
  return invoke<void>("app_confirm_exit");
}

/** Arms (or releases) the macOS wake lock that keeps the machine awake. */
export function setPreventSleep(active: boolean) {
  return invoke<void>("app_set_prevent_sleep", { active });
}

export function listenToEvents(onEvent: (event: QmuxEvent) => void): Promise<UnlistenFn> {
  return listen<QmuxEvent>("qmux-event", (event) => onEvent(event.payload));
}

/**
 * Tells the backend the qmux-event subscription is live. Until then the native
 * shortcut classifiers decline to consume chords, since the events they emit
 * would be dropped with nobody listening. The backend clears the flag itself
 * on every page navigation.
 */
export function markEventsListenerReady() {
  return invoke<void>("mark_events_listener_ready");
}

/** Acknowledges the native post-wake document event-loop health probe. */
export function acknowledgeInterfaceHealthProbe(generation: number) {
  return invoke<void>("acknowledge_interface_health_probe", { generation });
}

/** Verify and reattach the existing remote session; never starts another shell. */
export function reconnectPane(paneId: string) {
  return invoke<void>("pane_reconnect", { paneId });
}
