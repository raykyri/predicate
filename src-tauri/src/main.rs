mod adapters;
mod cli;
mod config;
mod connection_limit;
mod control_socket;
mod diagnostics;
mod encyclopedia;
mod events;
mod file_server;
mod global_task_launcher;
mod image_files;
mod launch_path;
mod menu_bar;
mod native_terminal;
mod persistence;
mod prompt_library;
mod pty;
mod publishing;
mod recovery;
mod research;
mod scrollback;
mod shell_jobs;
mod show_hide_shortcut;
mod sleep;
mod state;
mod thread_graph;
mod title_generation;
mod transcript;
mod turn_queue;
mod updater;
mod workspace;

use adapters::{
    MessageAnchor, SpawnAgentRequest, SpawnClaudeRequest, agent_fork as fork_agent_pane,
    agent_spawn as spawn_agent_pane, fork_agent_source,
};
use config::{QmuxConfig, RuntimeConfig};
use control_socket::start_control_socket;
use menu_bar::menu_bar_update;
use native_terminal::{
    native_terminal_action, native_terminal_focus, native_terminal_paste_approved_text,
    native_terminal_seed_settings, native_terminal_set_iframe_shortcut_fallback,
    native_terminal_set_keyboard_owner, native_terminal_set_layout,
    native_terminal_set_stage_backstop, native_terminal_set_web_overlay_region,
    native_terminal_set_web_pointer_claimed, native_terminal_theme_catalog,
    native_terminal_update_settings,
};
use pty::{
    InitialPaneSize, PaneActivity, PaneWriteOptions, attach_pane, close_worktree_pane, kill_pane,
    pane_activity as inspect_pane_activity, resize_pane, spawn_shell_pane, write_pane,
};
use research::{
    CreateResearchDocumentRequest, CreateResearchTreeRequest, ResearchBranchRemoval,
    ResearchFolderState, ResearchHighlight, ResearchHighlightAnchor, ResearchNode,
    ResearchNodeContent, ResearchTree, ResearchTreeDetail, ResearchTreeSummary,
    UpdateResearchDocumentRequest, UpdateResearchDocumentResult,
};
use show_hide_shortcut::{
    show_hide_shortcut_capture_set, show_hide_shortcut_get, show_hide_shortcut_set,
};
use sleep::SleepGuard;
use state::{
    AppState, BranchInfo, PaneInfo, PaneLayoutEntry, PaneSplitInfo, QueuedTurn, RecentSessionInfo,
    ShellAgentJobInfo,
};
use tauri::Manager;
use transcript::{
    TranscriptOption, Turn, list_agent_transcripts as list_agent_transcript_options,
    set_agent_transcript as repoint_agent_transcript,
};
use turn_queue::{
    AssignGlobalDraftRequest, AssignGlobalDraftResult, MoveQueuedAgentTurnRequest,
    MoveQueuedAgentTurnResult, QueueDeliveryAgentTurnRequest, QueueWaitAgentTurnRequest,
    RemoveQueuedAgentTurnRequest, RemoveQueuedAgentTurnResult, ReorderQueuedAgentTurnRequest,
    ReorderQueuedAgentTurnResult, SendNextQueuedAgentTurnResult, SubmitAgentTurnRequest,
    SubmitAgentTurnResult, move_queued_agent_turn, queue_delivery_agent_turn,
    queue_wait_agent_turn, remove_queued_agent_turn, reorder_queued_agent_turn,
    send_next_queued_agent_turn, set_agent_typing, submit_agent_turn, unpause_agent,
};
use workspace::{
    AgentInfo, AgentStatus, CreateGroupRequest, GroupInfo, LaunchOrigin, ResearchWorkspaceInfo,
    WorktreeStatus, acknowledge_agent, agent_worktree_signature, agent_worktree_status,
    clear_agent_working_status, create_group, create_research_workspace,
    ensure_default_research_workspace, move_research_workspace, remove_agent_worktree,
    remove_research_workspace, rename_group, rename_research_workspace, set_group_collapsed,
    set_group_dir, validate_launch_workspace,
};

fn handle_global_shortcut(
    app: &tauri::AppHandle,
    shortcut: &tauri_plugin_global_shortcut::Shortcut,
    event: tauri_plugin_global_shortcut::ShortcutEvent,
) {
    show_hide_shortcut::handle_global_shortcut(app, shortcut, event);
    global_task_launcher::handle_global_shortcut(app, shortcut, event);
}

/// Menu ids for the custom items installed by `customize_app_menu`.
#[cfg(desktop)]
const QUIT_MENU_ID: &str = "qmux-quit";
#[cfg(desktop)]
const NEW_WINDOW_MENU_ID: &str = "qmux-new-window";

/// Reworks the default menu for qmux's single-window behavior:
///
/// - Adds "New Window" to the otherwise-empty File menu. Since qmux owns one shared
///   session in one window, the action surfaces that window rather than constructing
///   a second webview over the same state.
/// - Strips the native "Close Window" items (⌘W on macOS, Alt+F4 elsewhere) so the
///   webview receives ⌘W itself; the frontend then routes ⌘W to close the active pane.
/// - On macOS, replaces the predefined "Quit" item with our own ⌘Q item. The native
///   item is hard-wired to Cocoa's `terminate:` selector, which tao does not intercept
///   (it implements `applicationWillTerminate:` but not `applicationShouldTerminate:`),
///   so ⌘Q would terminate the process instantly — bypassing both the `CloseRequested`
///   and `ExitRequested` handlers and quitting without confirmation even while agents
///   are running. Our replacement emits a `MenuEvent` we handle in `on_menu_event`.
///
/// Every other default item is preserved — notably the Edit menu that wires up ⌘C/⌘V/⌘A.
#[cfg(desktop)]
fn customize_app_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::MenuItemBuilder;
    use tauri::menu::{Menu, MenuItemKind};

    let menu = Menu::default(app.handle())?;
    for item in menu.items()? {
        let MenuItemKind::Submenu(submenu) = item else {
            continue;
        };
        let submenu_label = submenu.text()?.replace('&', "");
        if submenu_label == "File" {
            let new_window = MenuItemBuilder::with_id(NEW_WINDOW_MENU_ID, "New Window")
                .accelerator("CmdOrCtrl+N")
                .build(app)?;
            submenu.insert(&new_window, 0)?;
        }
        for (index, sub_item) in submenu.items()?.into_iter().enumerate() {
            let MenuItemKind::Predefined(predefined) = &sub_item else {
                continue;
            };
            // Match against the (mnemonic-stripped) label so platform copies line up.
            let label = predefined.text().unwrap_or_default().replace('&', "");
            if label == "Close Window" || label == "Close" {
                submenu.remove(predefined)?;
                continue;
            }
            // The macOS predefined Quit reads "Quit <app>"; preserve its label and
            // slot, but back it with our own handler instead of `terminate:`.
            #[cfg(target_os = "macos")]
            if label.starts_with("Quit") {
                let replacement = MenuItemBuilder::with_id(QUIT_MENU_ID, &label)
                    .accelerator("CmdOrCtrl+Q")
                    .build(app)?;
                submenu.remove(predefined)?;
                submenu.insert(&replacement, index)?;
            }
        }
    }
    app.set_menu(menu)?;
    Ok(())
}

/// Handles the custom application menu items.
#[cfg(desktop)]
fn handle_app_menu_event(app: &tauri::AppHandle, event: tauri::menu::MenuEvent) {
    if event.id().as_ref() == NEW_WINDOW_MENU_ID {
        show_main_window(app);
        return;
    }
    if event.id().as_ref() != QUIT_MENU_ID {
        return;
    }
    let Some(state) = app.try_state::<AppState>() else {
        app.exit(0);
        return;
    };
    if state.should_confirm_exit() {
        state.request_exit_confirmation();
    } else {
        app.exit(0);
    }
}

#[tauri::command]
fn get_runtime_config(state: tauri::State<'_, AppState>) -> RuntimeConfig {
    let mut runtime = state.config().runtime();
    // Surface the live file-server port so the frontend can identify token-bearing
    // file-server URLs and always sandbox them (see `isFileServerUrl`).
    runtime.file_server_port = state.file_server_port();
    runtime
}

// Commands below are marked `async` when they block: on this Tauri version a
// plain synchronous command runs on the macOS main thread, so any file I/O,
// subprocess spawn (git, pgrep/ps, pmset, open), PTY teardown, or sleep inside
// one freezes the entire UI — webview rendering, keyboard dispatch, and the
// native terminal surfaces — for its duration. `(async)` moves the same body to
// a worker thread; cheap in-memory getters/setters stay synchronous, and the
// native_terminal_* commands stay synchronous because their work must run on
// the main thread anyway (going async would only add a round-trip).
#[tauri::command(async)]
fn launcher_adapter_preference_get(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    Ok(persistence::load_preferences(&state.config().workspace_root)?.launcher_adapter_id)
}

/// Returns the stored OpenRouter API key (empty string when none is set). Kept in the
/// owner-only preferences file rather than webview localStorage — see AppPreferences.
#[tauri::command(async)]
fn openrouter_key_get(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(
        persistence::load_preferences(&state.config().workspace_root)?
            .open_router_key
            .unwrap_or_default(),
    )
}

/// Persists the OpenRouter API key. An empty/whitespace key clears it.
#[tauri::command(async)]
fn openrouter_key_set(state: tauri::State<'_, AppState>, key: String) -> Result<(), String> {
    let workspace_root = &state.config().workspace_root;
    let trimmed = key.trim();
    let open_router_key = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    persistence::update_preferences(workspace_root, move |preferences| {
        preferences.open_router_key = open_router_key;
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenRouterProxyResponse {
    status: u16,
    body: String,
}

/// Proxies an OpenRouter chat-completion request through the backend so the API key
/// never leaves the owner-only preferences file / the Rust process. The renderer
/// passes only the (non-secret) request payload; the key is read here and attached as
/// the `Authorization` header. The upstream status + raw body are returned so the
/// frontend keeps its existing response parsing and reasoning-effort retry logic.
/// Doing the request here rather than as a webview `fetch` keeps the key out of the
/// renderer heap on the request path and lets the webview CSP drop `openrouter.ai`
/// from `connect-src`.
#[tauri::command]
async fn openrouter_chat_completion(
    app: tauri::AppHandle,
    payload: serde_json::Value,
) -> Result<OpenRouterProxyResponse, String> {
    // Read the key under the state guard, then drop it before the await below.
    let key = {
        let state = app.state::<AppState>();
        persistence::load_preferences(&state.config().workspace_root)?
            .open_router_key
            .unwrap_or_default()
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err("No OpenRouter API key is configured.".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|err| format!("failed to build HTTP client: {err}"))?;
    let response = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .header("X-Title", "qmux")
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                "OpenRouter request timed out.".to_string()
            } else {
                format!("OpenRouter request failed: {err}")
            }
        })?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|err| format!("failed to read OpenRouter response: {err}"))?;
    Ok(OpenRouterProxyResponse { status, body })
}

#[tauri::command(async)]
fn launcher_adapter_preference_set(
    state: tauri::State<'_, AppState>,
    adapter_id: String,
) -> Result<(), String> {
    if !state
        .config()
        .runtime()
        .adapters
        .iter()
        .any(|adapter| adapter.id == adapter_id)
    {
        return Err(format!("unknown agent adapter '{adapter_id}'"));
    }

    persistence::update_preferences(&state.config().workspace_root, move |preferences| {
        preferences.launcher_adapter_id = Some(adapter_id);
    })
}

/// The frontend passes the active pane's project directory (its group dir, or
/// the group's base repo for worktrees) so project prompts follow the project
/// being worked on rather than the app instance.
fn prompt_project_path(project_dir: &Option<String>) -> Option<&std::path::Path> {
    project_dir
        .as_deref()
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(std::path::Path::new)
}

#[tauri::command(async)]
fn prompt_library_list(
    project_dir: Option<String>,
) -> Result<prompt_library::PromptLibrary, String> {
    prompt_library::list(prompt_project_path(&project_dir))
}

/// Creates or overwrites a saved prompt in `scope`. `previous_scope`/`previous_name`,
/// when they name a different prompt location, make this a rename and/or a move
/// between scopes (write new, then remove old).
#[tauri::command(async)]
fn prompt_library_save(
    scope: prompt_library::PromptScope,
    name: String,
    content: String,
    project_dir: Option<String>,
    previous_scope: Option<prompt_library::PromptScope>,
    previous_name: Option<String>,
    expected_modified_ms: Option<u64>,
) -> Result<prompt_library::SavedPrompt, String> {
    let previous = match (&previous_scope, &previous_name) {
        (Some(previous_scope), Some(previous_name)) => {
            Some((*previous_scope, previous_name.as_str()))
        }
        (None, None) => None,
        _ => return Err("previousScope and previousName must be passed together".to_string()),
    };
    prompt_library::save(
        prompt_project_path(&project_dir),
        scope,
        &name,
        &content,
        previous,
        expected_modified_ms,
    )
}

#[tauri::command(async)]
fn prompt_library_delete(
    scope: prompt_library::PromptScope,
    name: String,
    project_dir: Option<String>,
    expected_modified_ms: Option<u64>,
) -> Result<(), String> {
    prompt_library::delete(
        prompt_project_path(&project_dir),
        scope,
        &name,
        expected_modified_ms,
    )
}

/// Reveals a scope's prompts folder in the OS file manager, creating it (and the
/// project store's meta.json) first so a fresh library opens an empty folder
/// instead of erroring.
#[tauri::command(async)]
fn prompt_library_reveal(
    scope: prompt_library::PromptScope,
    project_dir: Option<String>,
) -> Result<(), String> {
    let dir = prompt_library::materialize_scope_dir(prompt_project_path(&project_dir), scope)?;
    open_path_in_file_manager(&dir)
}

#[cfg(target_os = "macos")]
fn open_path_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open {}: {err}", path.display()))
}

#[cfg(target_os = "linux")]
fn open_path_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open {}: {err}", path.display()))
}

#[cfg(target_os = "windows")]
fn open_path_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open {}: {err}", path.display()))
}

#[tauri::command]
fn active_tab_get(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    state.active_tab_id()
}

#[tauri::command]
fn active_tab_set(state: tauri::State<'_, AppState>, tab_id: Option<String>) -> Result<(), String> {
    state.set_active_tab_id(tab_id)
}

/// Surfaces a fatal startup error in a native dialog, for GUI (Finder/Dock)
/// launches that have no terminal to show the `eprintln`. Best-effort: if
/// `osascript` fails the message is still on stderr for a terminal launch.
#[cfg(target_os = "macos")]
fn notify_fatal_startup(message: &str) {
    // AppleScript string literals escape backslash and double-quote; embedded
    // newlines are fine inside the quoted literal.
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display dialog \"{escaped}\" with title \"qmux\" buttons {{\"Quit\"}} default button \"Quit\" with icon stop"
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status();
}

#[cfg(not(target_os = "macos"))]
fn notify_fatal_startup(_message: &str) {}

/// Surfaces a non-fatal startup warning (persisted state moved aside, entries
/// dropped during recovery) in a native dialog without blocking startup.
/// Best-effort: the message is already on stderr for terminal launches.
///
/// Uses the dialog plugin (as the folder picker does) rather than spawning
/// `osascript`: `show` is non-blocking and cross-platform, the message needs no
/// AppleScript escaping, and there is no unreaped child left as a zombie.
fn notify_startup_warning(app: &tauri::AppHandle, message: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

    app.dialog()
        .message(message)
        .title("qmux")
        .kind(MessageDialogKind::Warning)
        .show(|_| {});
}

/// Shows the native folder chooser in-process and returns the selected path, or
/// `None` when the user cancels. Blocks the calling thread, so callers must be
/// `#[tauri::command(async)]` (the panel itself is dispatched to the main thread
/// by the plugin).
fn pick_folder_dialog(app: &tauri::AppHandle, title: &str) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let mut dialog = app.dialog().file().set_title(title);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.set_parent(&window);
    }
    match dialog.blocking_pick_folder() {
        Some(path) => path
            .into_path()
            .map(|p| Some(p.to_string_lossy().into_owned()))
            .map_err(|err| format!("folder chooser returned an unusable path: {err}")),
        None => Ok(None),
    }
}

/// Opens a URL in the user's default external browser (or mail client). Only
/// http(s)/mailto are accepted; the URL is passed as a single argv to the OS opener
/// (no shell), so it can't trigger arbitrary scheme handlers or shell injection.
#[tauri::command(async)]
fn open_external_url(state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")) {
        return Err("refusing to open a non-http(s)/mailto URL externally".to_string());
    }
    // Never hand a token-bearing file-server URL to the OS opener: the default browser
    // would load it as a normal same-origin document (leaking the token into history and
    // to any local process) and could then read every sibling file under the pane's
    // roots. Legit loopback dev-server URLs on any *other* port still open externally.
    if let Some(port) = state.file_server_port()
        && is_file_server_url(&url, port)
    {
        return Err(
            "refusing to open a file-server URL externally (would leak the access token)"
                .to_string(),
        );
    }
    open_in_os_browser(&url)
}

/// Whether `url` is a loopback URL on the file server's port — i.e. a token-bearing
/// URL that must never leave the sandboxed overlay. Matches the frontend's
/// `isFileServerUrl` port check; done with prefix comparison to avoid a URL-parsing
/// dependency, mirroring the hand-rolled parsing in file_server.rs.
fn is_file_server_url(url: &str, port: u16) -> bool {
    url.starts_with(&format!("http://127.0.0.1:{port}/"))
        || url.starts_with(&format!("http://localhost:{port}/"))
}

#[cfg(target_os = "macos")]
fn open_in_os_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open externally: {err}"))
}

#[cfg(target_os = "linux")]
fn open_in_os_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open externally: {err}"))
}

#[cfg(target_os = "windows")]
fn open_in_os_browser(url: &str) -> Result<(), String> {
    // Avoid `cmd /C start`: Rust quotes argv by MSVCRT rules, but cmd.exe re-parses
    // `&|<>^` outside double quotes, so a URL like https://x/?a=1&b=2 would be split
    // (and the tail run as a separate command). Invoke the protocol handler directly
    // via rundll32 — no shell is involved, so the URL reaches the handler intact.
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open externally: {err}"))
}

#[tauri::command(async)]
fn list_claude_skills(state: tauri::State<'_, AppState>) -> Vec<adapters::claude::ClaudeSkill> {
    adapters::claude::list_skills(state.config())
}

// The list/refetch commands below are async on purpose, not because their
// reads are slow (each is a short clone under the model lock) but because sync
// commands run on the macOS main thread: the frontend calls them in bursts on
// every pane/agent/research event, exactly while backend threads (spawn
// binding, research settle/retirement, the persist snapshot) hold the model
// lock — so as sync commands they parked the main thread on that mutex and
// extended every research start/stop into a visible stall.
#[tauri::command]
async fn list_panes(state: tauri::State<'_, AppState>) -> Result<Vec<PaneInfo>, String> {
    state.list_panes()
}

#[tauri::command]
async fn list_groups(state: tauri::State<'_, AppState>) -> Result<Vec<GroupInfo>, String> {
    state.list_groups()
}

#[tauri::command]
fn list_research_workspaces(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ResearchWorkspaceInfo>, String> {
    state
        .list_research_workspaces()?
        .into_iter()
        .map(|group| {
            let dependencies = state.research_workspace_dependencies(&group.id)?;
            Ok(ResearchWorkspaceInfo {
                available: std::path::Path::new(&group.dir).is_dir(),
                tree_count: dependencies.tree_count,
                group,
            })
        })
        .collect()
}

#[tauri::command]
async fn ensure_default_research_workspace_command(
    state: tauri::State<'_, AppState>,
) -> Result<GroupInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || ensure_default_research_workspace(&state))
        .await
        .map_err(|err| format!("ensure_default_research_workspace task failed: {err}"))?
}

#[tauri::command]
async fn research_workspace_create_pick(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Option<GroupInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        match pick_folder_dialog(&app, "Select a research folder")? {
            Some(path) => create_research_workspace(&state, None, path).map(Some),
            None => Ok(None),
        }
    })
    .await
    .map_err(|err| format!("research_workspace_create_pick task failed: {err}"))?
}

#[tauri::command]
fn research_workspace_rename(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    name: Option<String>,
) -> Result<GroupInfo, String> {
    rename_research_workspace(&state, &workspace_id, name)
}

#[tauri::command]
async fn research_workspace_move_pick(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<Option<GroupInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        match pick_folder_dialog(&app, "Select a new location for this research folder")? {
            Some(path) => move_research_workspace(&state, &workspace_id, path).map(Some),
            None => Ok(None),
        }
    })
    .await
    .map_err(|err| format!("research_workspace_move_pick task failed: {err}"))?
}

#[tauri::command]
fn research_workspace_remove(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<String>, String> {
    remove_research_workspace(&state, &workspace_id)
}

#[tauri::command]
fn research_workspace_reveal(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<(), String> {
    let workspace = state
        .list_research_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| format!("research workspace {workspace_id} was not found"))?;
    let path = std::path::Path::new(&workspace.dir);
    if !path.is_dir() {
        return Err(format!(
            "research folder '{}' is unavailable",
            workspace.dir
        ));
    }
    open_path_in_file_manager(path)
}

#[tauri::command]
async fn list_agents(state: tauri::State<'_, AppState>) -> Result<Vec<AgentInfo>, String> {
    state.list_agents()
}

#[tauri::command]
fn list_shell_agent_jobs(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ShellAgentJobInfo>, String> {
    state.list_shell_agent_jobs()
}

#[tauri::command(async)]
fn list_recent_sessions(
    state: tauri::State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentSessionInfo>, String> {
    state.list_recent_sessions(limit.unwrap_or(12))
}

#[tauri::command(async)]
fn agent_branches(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<Vec<BranchInfo>, String> {
    state.list_agent_branches(&agent_id)
}

#[tauri::command]
async fn recent_session_resume(
    state: tauri::State<'_, AppState>,
    session_id: String,
    initial_size: Option<InitialPaneSize>,
) -> Result<PaneInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        recovery::resume_recent_session(&state, &session_id, initial_size)
    })
    .await
    .map_err(|err| format!("recent_session_resume task failed: {err}"))?
}

#[tauri::command(async)]
fn list_turns(
    state: tauri::State<'_, AppState>,
    agent_id: Option<String>,
) -> Result<Vec<Turn>, String> {
    state.list_turns(agent_id.as_deref())
}

#[tauri::command]
async fn list_thread_graphs(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<thread_graph::ThreadGraph>, String> {
    // Blocking: reads every thread's graph snapshot (disk on a cache miss) and
    // graphs retain full transcript nodes, so the response serializes the
    // workspace's whole fork history. As a sync command this ran on the main
    // thread and each full refresh stalled the UI for the duration.
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.list_thread_graphs())
        .await
        .map_err(|err| format!("list_thread_graphs task failed: {err}"))?
}

#[tauri::command]
async fn get_thread_graph(
    state: tauri::State<'_, AppState>,
    thread_id: String,
) -> Result<Option<thread_graph::ThreadGraph>, String> {
    // Blocking for the same reason as list_thread_graphs, scoped to one
    // thread: a long-running session's graph is still a large disk read and
    // serialization, and this refetch fires on every streaming turn burst.
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.thread_graph(&thread_id))
        .await
        .map_err(|err| format!("get_thread_graph task failed: {err}"))?
}

#[tauri::command]
async fn list_research_trees(
    state: tauri::State<'_, AppState>,
    include_archived: Option<bool>,
) -> Result<Vec<ResearchTreeSummary>, String> {
    if include_archived.unwrap_or(false) {
        state.list_research_trees_with_archived(true)
    } else {
        state.list_research_trees()
    }
}

#[tauri::command]
fn reorder_research_trees(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    archived: bool,
    tree_ids: Vec<String>,
) -> Result<(), String> {
    state.reorder_research_trees(&workspace_id, archived, tree_ids)
}

#[tauri::command]
fn list_research_folders(state: tauri::State<'_, AppState>) -> Result<ResearchFolderState, String> {
    state.research_folders()
}

#[tauri::command]
fn set_research_folders(
    state: tauri::State<'_, AppState>,
    folders: ResearchFolderState,
) -> Result<ResearchFolderState, String> {
    state.set_research_folders(folders)
}

#[tauri::command]
async fn list_research_activity(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ResearchNode>, String> {
    state.list_research_activity()
}

#[tauri::command]
async fn get_research_tree(
    state: tauri::State<'_, AppState>,
    tree_id: String,
) -> Result<ResearchTreeDetail, String> {
    state.research_tree(&tree_id)
}

fn fail_research_launch(state: &AppState, node_id: &str, pane_id: &str, error: String) -> String {
    match kill_pane(state, pane_id.to_string()) {
        Ok(()) => state.clear_last_closed_pane_for_pane(pane_id),
        Err(cleanup_error) => {
            eprintln!("qmux: failed to clean up unbound research pane {pane_id}: {cleanup_error}");
        }
    }
    let _ = state.fail_research_node(node_id, error.clone());
    error
}

/// A research run the user settled (cancelled) while its launch was still in
/// flight keeps its outcome — binding never resurrects it — but the launch has
/// produced a live pane nothing will ever retire: research panes are hidden
/// from the tab strip and the Cancel control is gone once the node is settled.
/// Reclaim it here, mirroring cancellation's own pane teardown.
fn reclaim_settled_research_launch(state: &AppState, node: &research::ResearchNode, pane_id: &str) {
    if !node.status.is_terminal() {
        return;
    }
    match kill_pane(state, pane_id.to_string()) {
        Ok(()) => state.clear_last_closed_pane_for_pane(pane_id),
        Err(err) => {
            if state.pane_exists(pane_id).unwrap_or(false) {
                eprintln!("qmux: failed to reclaim settled research pane {pane_id}: {err}");
            }
        }
    }
}

#[tauri::command]
async fn create_research_tree(
    state: tauri::State<'_, AppState>,
    request: CreateResearchTreeRequest,
) -> Result<ResearchTreeDetail, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Admission holds the workspace-mutation guard so a concurrent folder
        // removal can't slip between validation and the node insert —
        // the spawn itself runs unguarded (it's slow, and the Queued node
        // already marks the workspace busy).
        let detail = {
            let _guard = workspace::lock_research_workspace_mutations()?;
            validate_launch_workspace(&state, Some(&request.group_id), LaunchOrigin::Research)?;
            state.create_research_tree(request)?
        };
        let root = detail
            .nodes
            .first()
            .cloned()
            .ok_or_else(|| "new research tree has no root node".to_string())?;
        let workspace = match state.research_workspace_for_node(&root.id) {
            Ok(workspace) => workspace,
            Err(err) => {
                let _ = state.fail_research_node(&root.id, err.clone());
                remove_unlaunched_research_tree(&state, &detail.tree.id);
                return Err(err);
            }
        };
        match launch_fresh_research_run(
            &state,
            &root.id,
            &workspace,
            &root.adapter,
            root.model.clone(),
            root.effort.clone(),
            root.prompt.clone(),
        ) {
            Ok(_) => state.research_tree(&detail.tree.id),
            Err(err) => {
                remove_unlaunched_research_tree(&state, &detail.tree.id);
                Err(err)
            }
        }
    })
    .await
    .map_err(|err| format!("create_research_tree task failed: {err}"))?
}

/// Maps a research node's reasoning effort onto the launching adapter's own
/// launch-option key. Adapters without a reasoning-effort option launch with
/// their defaults.
fn research_launch_options(adapter: &str, effort: Option<&str>) -> serde_json::Value {
    match (adapter, effort) {
        ("claude", Some(effort)) => serde_json::json!({ "effort": effort }),
        ("codex", Some(effort)) => serde_json::json!({ "reasoningEffort": effort }),
        _ => serde_json::Value::Null,
    }
}

/// Launches a fresh (non-forked) agent run for an admitted research node and
/// binds the resulting pane. Shared by root-run creation and document
/// follow-ups. On failure the node is failed and any spawned pane reclaimed;
/// tree-level rollback stays with the caller.
fn launch_fresh_research_run(
    state: &AppState,
    node_id: &str,
    workspace: &workspace::GroupInfo,
    adapter: &str,
    model: Option<String>,
    effort: Option<String>,
    prompt: String,
) -> Result<research::ResearchNode, String> {
    let options = research_launch_options(adapter, effort.as_deref());
    // The custom launch instruction is read per launch (not cached at startup)
    // so a settings change applies from the very next run. It is applied to
    // the fully assembled prompt — after any document/conversation context
    // wrapper — so a leading slash command stays at the start of the message.
    // A damaged preferences file fails the launch, mirroring
    // update_preferences' refusal discipline, rather than silently launching
    // without the user's instruction.
    let prompt = match persistence::load_preferences(&state.config().workspace_root) {
        Ok(preferences) => research::prompt_with_research_launch_instruction(
            prompt,
            preferences.research_launch_instruction.as_deref(),
        ),
        Err(err) => {
            let _ = state.fail_research_node(node_id, err.clone());
            return Err(err);
        }
    };
    let spawn = SpawnAgentRequest {
        adapter_id: adapter.to_string(),
        prompt,
        group_id: Some(workspace.id.clone()),
        base_repo: Some(workspace.dir.clone()),
        base_ref: Some("HEAD".to_string()),
        cwd: None,
        model,
        initial_size: None,
        use_worktree: Some(false),
        options,
    };
    match spawn_agent_pane(state, spawn) {
        Ok(pane) => {
            let association = pane
                .agent_id
                .as_deref()
                .and_then(|agent_id| state.agent(agent_id).ok().flatten())
                .ok_or_else(|| "research agent was not recorded after launch".to_string())
                .and_then(|agent| {
                    state
                        .bind_research_node_run(node_id, &agent, &pane.id)
                        .map(|node| (agent, node))
                });
            match association {
                Ok((agent, node)) => {
                    if node.status.is_terminal() {
                        // Cancelled while the spawn was in flight: the
                        // outcome stands and the pane is reclaimed, so
                        // there is nothing to announce.
                        reclaim_settled_research_launch(state, &node, &pane.id);
                    } else {
                        // Fresh spawns go through launch(), which emits no event
                        // (launcher spawns assume a frontend caller holds the
                        // pane). Nothing holds this one, so announce it or the
                        // pane never enters the frontend list: Background
                        // activity can't show it and "Open terminal" misses.
                        state.emit(events::QmuxEvent::new(
                            "agent.spawned",
                            Some(pane.id.clone()),
                            Some(agent.id.clone()),
                            serde_json::json!({
                                "agent": agent,
                                "pane": pane,
                                "source": "research",
                            }),
                        ));
                        state.schedule_research_startup_watchdog(agent.id.clone());
                    }
                    state.research_node(node_id)
                }
                Err(err) => Err(fail_research_launch(state, node_id, &pane.id, err)),
            }
        }
        Err(err) => {
            let _ = state.fail_research_node(node_id, err.clone());
            Err(err)
        }
    }
}

#[tauri::command]
async fn create_research_document(
    state: tauri::State<'_, AppState>,
    request: CreateResearchDocumentRequest,
) -> Result<ResearchTreeDetail, String> {
    let state = state.inner().clone();
    // Blocking: the document body is written to its response snapshot (fsync'd
    // file IO) before the records commit.
    tauri::async_runtime::spawn_blocking(move || {
        // Same admission as create_research_tree: the insert must be atomic
        // with the workspace checks or a concurrent folder removal could
        // detach the workspace out from under the new records. There is no
        // run to launch, so admission is the whole command.
        let _guard = workspace::lock_research_workspace_mutations()?;
        validate_launch_workspace(&state, Some(&request.group_id), LaunchOrigin::Research)?;
        state.create_research_document(request)
    })
    .await
    .map_err(|err| format!("create_research_document task failed: {err}"))?
}

#[tauri::command]
async fn export_pane_to_research(
    state: tauri::State<'_, AppState>,
    request: research::ExportPaneToResearchRequest,
) -> Result<ResearchTreeDetail, String> {
    let state = state.inner().clone();
    // Blocking: the export reads the source transcript (twice, for a stable
    // parse) and fsyncs the conversation snapshot.
    tauri::async_runtime::spawn_blocking(move || {
        // The slow work — transcript reads, sanitization, the snapshot write
        // — runs unguarded, like create_research_tree's spawn: only
        // admission needs atomicity with workspace mutations, and a failure
        // after prepare strands at most an orphan snapshot.
        let prepared = state.prepare_pane_export(&request.pane_id)?;
        let committed = {
            let _guard = workspace::lock_research_workspace_mutations()?;
            validate_launch_workspace(&state, Some(&request.group_id), LaunchOrigin::Research)
                .and_then(|_| state.commit_pane_export(&prepared, request.group_id, request.title))
        };
        if committed.is_err() {
            // Redundant after a commit-side admission failure (the removal is
            // idempotent), but validation failures never reach commit.
            state.discard_pane_export(&prepared);
        }
        committed
    })
    .await
    .map_err(|err| format!("export_pane_to_research task failed: {err}"))?
}

#[tauri::command]
async fn update_research_document(
    state: tauri::State<'_, AppState>,
    request: UpdateResearchDocumentRequest,
) -> Result<UpdateResearchDocumentResult, String> {
    let state = state.inner().clone();
    // Blocking: a body edit atomically replaces and fsyncs the durable response
    // snapshot before the in-memory metadata is announced. Serialize with
    // research-folder replacement/removal as well, or another window could
    // detach the tree after validation but before the edit commits.
    tauri::async_runtime::spawn_blocking(move || {
        let _workspace_guard = workspace::lock_research_workspace_mutations()?;
        state.update_research_document(request)
    })
    .await
    .map_err(|err| format!("update_research_document task failed: {err}"))?
}

#[tauri::command]
async fn read_markdown_document_file(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        research::read_markdown_document_file(std::path::Path::new(&path))
    })
    .await
    .map_err(|err| format!("read_markdown_document_file task failed: {err}"))?
}

/// The encyclopedia surface's one-stop snapshot: settings, pages on disk,
/// pending source count, and whether an update run is in flight. Reaps a
/// finished run first, so fetching status is also what settles a run whose
/// completion event the frontend acted on (advancing the cursor and closing
/// the run's hidden pane).
#[tauri::command]
async fn encyclopedia_status(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<encyclopedia::EncyclopediaStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let record = state.reap_encyclopedia_run(&workspace_id)?;
        encyclopedia_status_snapshot(&state, &workspace_id, record)
    })
    .await
    .map_err(|err| format!("encyclopedia_status task failed: {err}"))?
}

#[tauri::command]
async fn encyclopedia_set_settings(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    enabled: bool,
    auto_update: bool,
) -> Result<encyclopedia::EncyclopediaStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let record = state.set_encyclopedia_settings(&workspace_id, enabled, auto_update)?;
        if enabled {
            // Make the folder visible in the workspace as soon as the feature
            // is on, so "a special folder called encyclopedia" exists even
            // before the first generated page lands.
            let workspace = state
                .group(&workspace_id)?
                .ok_or_else(|| format!("workspace {workspace_id} was not found"))?;
            let dir = encyclopedia::encyclopedia_dir(std::path::Path::new(&workspace.dir));
            std::fs::create_dir_all(&dir)
                .map_err(|err| format!("failed to create the encyclopedia folder: {err}"))?;
        }
        encyclopedia_status_snapshot(&state, &workspace_id, record)
    })
    .await
    .map_err(|err| format!("encyclopedia_set_settings task failed: {err}"))?
}

/// Launches an encyclopedia update run: digests the workspace's new chat
/// material into a launch prompt and starts an agent in the workspace
/// directory to fold it into `encyclopedia/`. With `full`, digests from the
/// beginning instead of the cursor (a rebuild).
#[tauri::command]
async fn encyclopedia_update(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    adapter: String,
    model: Option<String>,
    full: bool,
) -> Result<encyclopedia::EncyclopediaStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Settle any finished previous run first so its cursor advance is
        // visible to the source collection below.
        let record = state.reap_encyclopedia_run(&workspace_id)?;
        if !record.enabled {
            return Err("the encyclopedia is not enabled for this workspace".to_string());
        }
        if record.active_run.is_some() {
            return Err("an encyclopedia update is already running".to_string());
        }
        let workspace = {
            let _guard = workspace::lock_research_workspace_mutations()?;
            validate_launch_workspace(&state, Some(&workspace_id), LaunchOrigin::Research)?
                .ok_or_else(|| format!("workspace {workspace_id} was not found"))?
        };
        let since = if full { 0 } else { record.last_generated_at };
        // The cutoff is captured before source collection: material that
        // lands while the run executes has a later source time and stays
        // pending for the next update.
        let cutoff = encyclopedia::now_ms();
        let refs = state.encyclopedia_source_nodes(&workspace_id, since)?;
        let sources = load_encyclopedia_sources(&state, refs);
        let prompt = encyclopedia::encyclopedia_update_prompt(&sources, full)?;
        let dir = encyclopedia::encyclopedia_dir(std::path::Path::new(&workspace.dir));
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("failed to create the encyclopedia folder: {err}"))?;
        let spawn = SpawnAgentRequest {
            adapter_id: adapter,
            prompt,
            group_id: Some(workspace.id.clone()),
            base_repo: Some(workspace.dir.clone()),
            base_ref: Some("HEAD".to_string()),
            cwd: None,
            model,
            initial_size: None,
            use_worktree: Some(false),
            options: serde_json::Value::Null,
        };
        let pane = match spawn_agent_pane(&state, spawn) {
            Ok(pane) => pane,
            Err(err) => {
                state.set_encyclopedia_error(&workspace_id, err.clone());
                return Err(err);
            }
        };
        let launched = pane
            .agent_id
            .as_deref()
            .and_then(|agent_id| state.agent(agent_id).ok().flatten())
            .ok_or_else(|| "encyclopedia agent was not recorded after launch".to_string())
            .and_then(|agent| {
                state
                    .begin_encyclopedia_run(
                        &workspace_id,
                        encyclopedia::EncyclopediaRun {
                            agent_id: agent.id.clone(),
                            pane_id: pane.id.clone(),
                            started_at: cutoff,
                            cutoff,
                        },
                    )
                    .map(|record| (agent, record))
            });
        match launched {
            Ok((agent, record)) => {
                // Like fresh research spawns: no frontend caller holds this
                // pane, so announce it or Background activity misses it.
                state.emit(events::QmuxEvent::new(
                    "agent.spawned",
                    Some(pane.id.clone()),
                    Some(agent.id.clone()),
                    serde_json::json!({
                        "agent": agent,
                        "pane": pane,
                        "source": "encyclopedia",
                    }),
                ));
                encyclopedia_status_snapshot(&state, &workspace_id, record)
            }
            Err(err) => {
                match kill_pane(&state, pane.id.clone()) {
                    Ok(()) => state.clear_last_closed_pane_for_pane(&pane.id),
                    Err(cleanup_error) => eprintln!(
                        "qmux: failed to clean up unbound encyclopedia pane {}: {cleanup_error}",
                        pane.id
                    ),
                }
                state.set_encyclopedia_error(&workspace_id, err.clone());
                Err(err)
            }
        }
    })
    .await
    .map_err(|err| format!("encyclopedia_update task failed: {err}"))?
}

/// Cancels an in-flight update run without advancing the cursor.
#[tauri::command]
async fn encyclopedia_cancel(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<encyclopedia::EncyclopediaStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let record = state.cancel_encyclopedia_run(&workspace_id)?;
        encyclopedia_status_snapshot(&state, &workspace_id, record)
    })
    .await
    .map_err(|err| format!("encyclopedia_cancel task failed: {err}"))?
}

#[tauri::command]
async fn encyclopedia_read_page(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    file_name: String,
) -> Result<encyclopedia::EncyclopediaPageContent, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let workspace = state
            .group(&workspace_id)?
            .ok_or_else(|| format!("workspace {workspace_id} was not found"))?;
        let dir = encyclopedia::encyclopedia_dir(std::path::Path::new(&workspace.dir));
        encyclopedia::read_encyclopedia_page(&dir, &file_name)
    })
    .await
    .map_err(|err| format!("encyclopedia_read_page task failed: {err}"))?
}

fn encyclopedia_status_snapshot(
    state: &AppState,
    workspace_id: &str,
    record: encyclopedia::WorkspaceEncyclopedia,
) -> Result<encyclopedia::EncyclopediaStatus, String> {
    let workspace = state
        .group(workspace_id)?
        .ok_or_else(|| format!("workspace {workspace_id} was not found"))?;
    let dir = encyclopedia::encyclopedia_dir(std::path::Path::new(&workspace.dir));
    let pages = encyclopedia::list_encyclopedia_pages(&dir)?;
    let pending_source_count = state
        .encyclopedia_source_nodes(workspace_id, record.last_generated_at)?
        .len();
    Ok(encyclopedia::EncyclopediaStatus {
        workspace_id: workspace_id.to_string(),
        enabled: record.enabled,
        auto_update: record.auto_update,
        updating: record.active_run.is_some(),
        active_agent_id: record.active_run.map(|run| run.agent_id),
        last_generated_at: record.last_generated_at,
        pending_source_count,
        pages,
        last_error: record.last_error,
    })
}

/// Reads each source node's durable response snapshot (falling back to the
/// live transcript for a snapshot that never landed) and flattens it into
/// digest text. Sources whose content is unavailable are skipped rather than
/// failing the whole update.
fn load_encyclopedia_sources(
    state: &AppState,
    refs: Vec<(ResearchNode, String)>,
) -> Vec<encyclopedia::EncyclopediaSource> {
    refs.into_iter()
        .filter_map(|(node, tree_title)| {
            let turns = research::read_response_snapshot(&state.config().workspace_root, &node.id)
                .ok()
                .flatten()
                .or_else(|| {
                    let ancestor_prompts = state
                        .research_node_ancestor_prompts(&node.id)
                        .unwrap_or_default();
                    research::load_transcript_response(state.config(), &node, &ancestor_prompts)
                        .ok()
                })?;
            let (kind, include_user) = match node.kind {
                research::ResearchNodeKind::Run => {
                    (encyclopedia::EncyclopediaSourceKind::Chat, false)
                }
                research::ResearchNodeKind::Document => {
                    (encyclopedia::EncyclopediaSourceKind::Document, false)
                }
                research::ResearchNodeKind::Conversation => {
                    (encyclopedia::EncyclopediaSourceKind::Conversation, true)
                }
            };
            let body = encyclopedia::turns_digest_text(&turns, include_user);
            let prompt = if matches!(node.kind, research::ResearchNodeKind::Run) {
                node.prompt.clone()
            } else {
                String::new()
            };
            let title = node
                .title
                .clone()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(tree_title);
            Some(encyclopedia::EncyclopediaSource {
                kind,
                title,
                citation: encyclopedia::citation_path(&node.tree_id, &node.id),
                prompt,
                body,
            })
        })
        .collect()
}

/// Reads a pasted image referenced by a transcript "[Image: source: <path>]"
/// marker and returns it as a data: URL (the webview CSP blocks file paths in
/// <img> tags). The backend enforces the home-directory confinement, raster
/// extension allowlist, regular-file requirement, and byte cap before any
/// content reaches the webview.
#[tauri::command]
async fn read_transcript_image(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        image_files::read_transcript_image(std::path::Path::new(&path))
    })
    .await
    .map_err(|err| format!("read_transcript_image task failed: {err}"))?
}

/// The tree is committed before its root run launches so a crash mid-launch is
/// recoverable, but a root that never launched holds nothing durable. Leaving
/// it behind on a launch failure accumulated dead entries the caller could not
/// even identify — the command returns the error, not the tree id — while the
/// dialog keeps the prompt for a retry. Best-effort: if the removal itself
/// fails, the failed tree remains visible (and removable) in the sidebar.
fn remove_unlaunched_research_tree(state: &AppState, tree_id: &str) {
    if let Err(err) = state.remove_research_tree(tree_id) {
        eprintln!("qmux: failed to remove unlaunched research tree {tree_id}: {err}");
    }
}

#[tauri::command]
async fn get_research_node_content(
    state: tauri::State<'_, AppState>,
    node_id: String,
) -> Result<ResearchNodeContent, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut content = state.research_node_content(&node_id)?;
        // A corrupt or oversized snapshot must not wedge the node: fall back to
        // the transcript exactly as if no snapshot existed, keeping the read
        // failure only as diagnostic context if nothing else is viewable.
        let snapshot_error = match research::read_response_snapshot_with_revision(
            &state.config().workspace_root,
            &node_id,
        ) {
            Ok(Some(snapshot)) => {
                content.response_revision = Some(snapshot.revision);
                content.turns = snapshot.turns;
                return Ok(content);
            }
            Ok(None) => None,
            Err(err) => {
                eprintln!("qmux: unreadable research response snapshot {node_id}: {err}");
                Some(err)
            }
        };
        if content.node.transcript_path.is_some()
            && matches!(
                content.node.status,
                research::ResearchNodeStatus::Complete
                    | research::ResearchNodeStatus::Failed
                    | research::ResearchNodeStatus::Cancelled
            )
        {
            let ancestor_prompts = state
                .research_node_ancestor_prompts(&node_id)
                .unwrap_or_default();
            match research::load_transcript_response(
                state.config(),
                &content.node,
                &ancestor_prompts,
            ) {
                Ok(turns) => content.turns = turns,
                // No snapshot, no live turns, and the adapter transcript is
                // unreadable: return the node with the failure recorded rather
                // than erroring, which would wedge the workspace on a retry
                // loop that can never succeed and hide the node entirely.
                Err(err) if content.turns.is_empty() => {
                    content.source_error = Some(match snapshot_error {
                        Some(snapshot_error) => format!("{snapshot_error}; {err}"),
                        None => err,
                    });
                }
                Err(_) => {}
            }
        } else if content.turns.is_empty() {
            content.source_error = snapshot_error;
        }
        Ok(content)
    })
    .await
    .map_err(|err| format!("get_research_node_content task failed: {err}"))?
}

#[tauri::command]
async fn fork_research_node(
    state: tauri::State<'_, AppState>,
    parent_node_id: String,
    prompt: String,
    publication_proposal: Option<research::ResearchPublicationProposal>,
    query_anchor: Option<research::ResearchHighlightAnchor>,
    inline: Option<bool>,
) -> Result<ResearchNode, String> {
    let state = state.inner().clone();
    let inline = inline.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || {
        if inline && publication_proposal.is_some() {
            return Err("community proposals become branches, not inline follow-ups".to_string());
        }
        // Same admission guard as create_research_tree: the Queued child must
        // be admitted atomically with the workspace checks, or a concurrent
        // folder removal could invalidate its workspace before the fork.
        let (parent, workspace, child) = {
            let _guard = workspace::lock_research_workspace_mutations()?;
            let parent = state.research_node(&parent_node_id)?;
            let workspace = state.research_workspace_for_node(&parent_node_id)?;
            validate_launch_workspace(&state, Some(&workspace.id), LaunchOrigin::Research)?;
            let child = match publication_proposal {
                Some(proposal) => {
                    state.create_research_child_for_proposal(&parent_node_id, prompt, proposal)?
                }
                None => {
                    state.create_research_child(&parent_node_id, prompt, query_anchor, inline)?
                }
            };
            (parent, workspace, child)
        };
        // A highlight-targeted follow-up sends the quoted passage with the
        // question. Only the sent prompt carries the quote — the child's
        // displayed prompt stays the bare question, which boundary matching
        // still finds as a normalized substring of the sent prompt.
        let question = match &child.query_anchor {
            Some(anchor) => research::query_followup_prompt(&anchor.exact, &child.prompt),
            None => child.prompt.clone(),
        };
        // Exhaustive on purpose: each kind must pick its launch path
        // explicitly, so a new kind (or lifting a refusal in
        // create_research_child) forces a decision here instead of falling
        // into the session-fork branch without a checkpoint.
        match parent.kind {
            research::ResearchNodeKind::Document => {
                // A document has no session to fork. Its follow-up launches a
                // fresh run whose prompt carries the document as context; the
                // child's displayed prompt stays the bare question (the
                // response boundary still matches it as a substring of the
                // sent prompt).
                let launch_prompt = state.research_document_followup_prompt(&parent.id, &question);
                let launch_prompt = match launch_prompt {
                    Ok(launch_prompt) => launch_prompt,
                    Err(err) => {
                        let _ = state.fail_research_node(&child.id, err.clone());
                        return Err(err);
                    }
                };
                return launch_fresh_research_run(
                    &state,
                    &child.id,
                    &workspace,
                    &child.adapter,
                    child.model.clone(),
                    child.effort.clone(),
                    launch_prompt,
                );
            }
            research::ResearchNodeKind::Conversation => {
                // An exported conversation is severed from its source session
                // — there is nothing to fork. Its follow-up launches a fresh
                // run whose prompt carries the serialized conversation as
                // context; the child's displayed prompt stays the bare
                // question (the response boundary still matches it as a
                // substring of the sent prompt).
                let launch_prompt =
                    state.research_conversation_followup_prompt(&parent.id, &question);
                let launch_prompt = match launch_prompt {
                    Ok(launch_prompt) => launch_prompt,
                    Err(err) => {
                        let _ = state.fail_research_node(&child.id, err.clone());
                        return Err(err);
                    }
                };
                return launch_fresh_research_run(
                    &state,
                    &child.id,
                    &workspace,
                    &child.adapter,
                    child.model.clone(),
                    child.effort.clone(),
                    launch_prompt,
                );
            }
            research::ResearchNodeKind::Run => {}
        }
        // Run follow-ups fork the parent session, bypassing
        // launch_fresh_research_run, so the custom launch instruction is
        // applied to the sent question here. The child's displayed prompt
        // stays the bare question — still a normalized substring of the sent
        // prompt, so response-boundary matching keeps working.
        let question = match persistence::load_preferences(&state.config().workspace_root) {
            Ok(preferences) => research::prompt_with_research_launch_instruction(
                question,
                preferences.research_launch_instruction.as_deref(),
            ),
            Err(err) => {
                let _ = state.fail_research_node(&child.id, err.clone());
                return Err(err);
            }
        };
        let live_source = parent
            .agent_id
            .as_deref()
            .and_then(|agent_id| state.agent(agent_id).ok().flatten());
        let mut source = match live_source {
            Some(source) => source,
            None => {
                let session_id = match parent.native_session_id.clone() {
                    Some(session_id) => session_id,
                    None => {
                        let err = "the parent research session has no native session id to fork"
                            .to_string();
                        let _ = state.fail_research_node(&child.id, err.clone());
                        return Err(err);
                    }
                };
                AgentInfo {
                    id: parent
                        .agent_id
                        .clone()
                        .unwrap_or_else(|| format!("research-source-{}", parent.id)),
                    group_id: parent.group_id.clone(),
                    adapter: parent.adapter.clone(),
                    worktree_dir: parent.worktree_dir.clone(),
                    branch: None,
                    pane_id: None,
                    orphaned_queue_pane_id: None,
                    session_id: Some(session_id.clone()),
                    transcript_path: parent.transcript_path.clone(),
                    status: AgentStatus::Done,
                    model: parent.model.clone(),
                    effort: parent.effort.clone(),
                    parent_id: None,
                    fork_point: None,
                    root_session_id: Some(session_id),
                    thread_id: None,
                    branch_id: None,
                    paused: false,
                    created_at: parent.created_at,
                }
            }
        };
        // Native checkpoints come from the parent run, but execution ownership
        // and cwd always come from the tree's current durable workspace.
        source.group_id = workspace.id;
        source.worktree_dir = workspace.dir;
        // The follow-up runs at the child's (inherited) effort, applied by the
        // adapter's fork path the same way `model` is re-applied.
        source.effort = child.effort.clone();
        match fork_agent_source(&state, &source, false, true, Some(&question)) {
            Ok(pane) => {
                let association = pane
                    .agent_id
                    .as_deref()
                    .and_then(|agent_id| state.agent(agent_id).ok().flatten())
                    .ok_or_else(|| "forked research agent was not recorded".to_string())
                    .and_then(|agent| state.bind_research_node_run(&child.id, &agent, &pane.id));
                match association {
                    Ok(node) if node.status.is_terminal() => {
                        // Cancelled while the fork was in flight: keep the
                        // settled outcome, reclaim the fresh pane, and hand
                        // back the node as it stands after the teardown.
                        reclaim_settled_research_launch(&state, &node, &pane.id);
                        state.research_node(&child.id)
                    }
                    Ok(node) => {
                        // Forks usually land in an already-trusted directory,
                        // but login/update gates are just as hook-invisible as
                        // the trust dialog — arm the same startup watchdog as
                        // fresh runs.
                        if let Some(agent_id) = node.agent_id.clone() {
                            state.schedule_research_startup_watchdog(agent_id);
                        }
                        Ok(node)
                    }
                    Err(err) => Err(fail_research_launch(&state, &child.id, &pane.id, err)),
                }
            }
            Err(err) => {
                let _ = state.fail_research_node(&child.id, err.clone());
                Err(err)
            }
        }
    })
    .await
    .map_err(|err| format!("fork_research_node task failed: {err}"))?
}

#[tauri::command]
async fn cancel_research_node(
    state: tauri::State<'_, AppState>,
    node_id: String,
) -> Result<ResearchNode, String> {
    let state = state.inner().clone();
    // Blocking: cancellation kills the run's pane (process wait + teardown).
    tauri::async_runtime::spawn_blocking(move || state.cancel_research_node(&node_id))
        .await
        .map_err(|err| format!("cancel_research_node task failed: {err}"))?
}

#[tauri::command]
fn rename_research_tree(
    state: tauri::State<'_, AppState>,
    tree_id: String,
    title: String,
) -> Result<ResearchTree, String> {
    state.rename_research_tree(&tree_id, title)
}

#[tauri::command]
fn rename_research_node(
    state: tauri::State<'_, AppState>,
    node_id: String,
    title: String,
) -> Result<ResearchNode, String> {
    state.set_research_node_title(&node_id, title)
}

#[tauri::command]
async fn create_research_highlight(
    state: tauri::State<'_, AppState>,
    node_id: String,
    anchor: ResearchHighlightAnchor,
) -> Result<ResearchHighlight, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.create_research_highlight(&node_id, anchor))
        .await
        .map_err(|err| format!("research highlight task failed: {err}"))?
}

#[tauri::command]
async fn remove_research_highlight(
    state: tauri::State<'_, AppState>,
    node_id: String,
    highlight_id: String,
) -> Result<ResearchHighlight, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state.remove_research_highlight(&node_id, &highlight_id)
    })
    .await
    .map_err(|err| format!("research highlight task failed: {err}"))?
}

#[tauri::command]
async fn remove_research_highlights(
    state: tauri::State<'_, AppState>,
    node_id: String,
    highlight_ids: Vec<String>,
) -> Result<Vec<ResearchHighlight>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state.remove_research_highlights(&node_id, &highlight_ids)
    })
    .await
    .map_err(|err| format!("research highlight task failed: {err}"))?
}

#[tauri::command]
async fn mark_research_tree_viewed(
    state: tauri::State<'_, AppState>,
    tree_id: String,
) -> Result<ResearchTree, String> {
    state.mark_research_tree_viewed(&tree_id)
}

#[tauri::command]
fn archive_research_tree(
    state: tauri::State<'_, AppState>,
    tree_id: String,
) -> Result<ResearchTree, String> {
    state.archive_research_tree(&tree_id)
}

#[tauri::command]
fn restore_research_tree(
    state: tauri::State<'_, AppState>,
    tree_id: String,
) -> Result<ResearchTree, String> {
    state.restore_research_tree(&tree_id)
}

#[tauri::command]
fn remove_research_tree(state: tauri::State<'_, AppState>, tree_id: String) -> Result<(), String> {
    state.remove_research_tree(&tree_id)
}

#[tauri::command]
fn remove_research_branch(
    state: tauri::State<'_, AppState>,
    node_id: String,
) -> Result<ResearchBranchRemoval, String> {
    state.remove_research_branch(&node_id)
}

#[tauri::command]
fn list_agent_turn_queue(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<Vec<QueuedTurn>, String> {
    state.agent_queued_turns(&agent_id)
}

// Async so it runs off the main thread: building the picker reads the head of
// up to 30 transcript files (and walks the Codex sessions tree), which froze
// the UI for the duration when run as a synchronous command.
#[tauri::command(async)]
fn list_agent_transcripts(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<Vec<TranscriptOption>, String> {
    list_agent_transcript_options(&state, &agent_id)
}

#[tauri::command(async)]
fn set_agent_transcript(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    path: Option<String>,
) -> Result<AgentInfo, String> {
    repoint_agent_transcript(&state, &agent_id, path.as_deref())
}

#[tauri::command]
async fn group_create(
    state: tauri::State<'_, AppState>,
    request: CreateGroupRequest,
) -> Result<GroupInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || create_group(&state, request))
        .await
        .map_err(|err| format!("group_create task failed: {err}"))?
}

#[tauri::command]
fn group_remove(state: tauri::State<'_, AppState>, group_id: String) -> Result<(), String> {
    if state
        .group(&group_id)?
        .is_some_and(|group| group.scope != workspace::WorkspaceScope::Terminal)
    {
        return Err(
            "use the research workspace removal command for Research workspaces".to_string(),
        );
    }
    state.remove_group(&group_id)
}

#[tauri::command]
fn group_rename(
    state: tauri::State<'_, AppState>,
    group_id: String,
    name: Option<String>,
) -> Result<GroupInfo, String> {
    rename_group(&state, &group_id, name)
}

#[tauri::command]
fn group_reorder(
    state: tauri::State<'_, AppState>,
    group_ids: Vec<String>,
) -> Result<Vec<GroupInfo>, String> {
    state.reorder_groups(group_ids)
}

#[tauri::command]
fn group_set_collapsed(
    state: tauri::State<'_, AppState>,
    group_id: String,
    collapsed: bool,
) -> Result<GroupInfo, String> {
    set_group_collapsed(&state, &group_id, collapsed)
}

// Commands whose bodies can block for seconds or longer run on tokio's
// dedicated blocking pool (spawn_blocking, which grows to hundreds of threads)
// rather than as `(async)` sync bodies: the latter execute inline on the
// runtime's core workers (one per CPU), so a handful of concurrent git
// checkouts — or a folder picker parked open — could otherwise starve every
// other command, including pane writes and turn submits.
#[tauri::command]
async fn group_create_pick(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    after_group_id: Option<String>,
) -> Result<Option<GroupInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        match pick_folder_dialog(&app, "Select the group directory")? {
            Some(path) => create_group(
                &state,
                CreateGroupRequest {
                    name: None,
                    dir: Some(path),
                    after_group_id,
                    base_repo: None,
                    base_ref: None,
                },
            )
            .map(Some),
            None => Ok(None),
        }
    })
    .await
    .map_err(|err| format!("group_create_pick task failed: {err}"))?
}

#[tauri::command]
async fn group_pick_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    group_id: String,
) -> Result<Option<GroupInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        match pick_folder_dialog(&app, "Select the group directory")? {
            Some(path) => set_group_dir(&state, &group_id, path).map(Some),
            None => Ok(None),
        }
    })
    .await
    .map_err(|err| format!("group_pick_dir task failed: {err}"))?
}

#[tauri::command]
async fn spawn_shell(
    state: tauri::State<'_, AppState>,
    initial_size: Option<InitialPaneSize>,
    source_pane_id: Option<String>,
    group_id: Option<String>,
) -> Result<PaneInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        validate_launch_workspace(&state, group_id.as_deref(), LaunchOrigin::Terminal)?;
        spawn_shell_pane(
            &state,
            initial_size,
            source_pane_id.as_deref(),
            group_id.as_deref(),
        )
    })
    .await
    .map_err(|err| format!("spawn_shell task failed: {err}"))?
}

#[tauri::command(async)]
fn use_login_shell_get(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(
        persistence::load_preferences(&state.config().workspace_root)?
            .use_login_shell
            .unwrap_or(true),
    )
}

#[tauri::command(async)]
fn use_login_shell_set(state: tauri::State<'_, AppState>, enabled: bool) -> Result<(), String> {
    persistence::update_preferences(&state.config().workspace_root, |preferences| {
        preferences.use_login_shell = Some(enabled);
    })
}

#[tauri::command(async)]
fn worktree_location_get(
    state: tauri::State<'_, AppState>,
) -> Result<persistence::WorktreeLocation, String> {
    Ok(
        persistence::load_preferences(&state.config().workspace_root)?
            .worktree_location
            .unwrap_or_default(),
    )
}

#[tauri::command(async)]
fn worktree_location_set(
    state: tauri::State<'_, AppState>,
    location: persistence::WorktreeLocation,
) -> Result<(), String> {
    persistence::update_preferences(&state.config().workspace_root, |preferences| {
        preferences.worktree_location = Some(location);
    })
}

/// Returns the stored research launch instruction (empty string when unset).
/// The backend owns this preference — see AppPreferences — because research
/// launch prompts are assembled on the Rust side.
#[tauri::command(async)]
fn research_launch_instruction_get(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(
        persistence::load_preferences(&state.config().workspace_root)?
            .research_launch_instruction
            .unwrap_or_default(),
    )
}

/// Persists the research launch instruction. An empty/whitespace value clears
/// it (launches send prompts unchanged); a value over the byte cap is refused.
#[tauri::command(async)]
fn research_launch_instruction_set(
    state: tauri::State<'_, AppState>,
    instruction: String,
) -> Result<(), String> {
    let instruction = research::sanitized_research_launch_instruction(&instruction)?;
    persistence::update_preferences(&state.config().workspace_root, move |preferences| {
        preferences.research_launch_instruction = instruction;
    })
}

#[tauri::command]
async fn agent_spawn(
    state: tauri::State<'_, AppState>,
    request: SpawnAgentRequest,
) -> Result<PaneInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        validate_launch_workspace(&state, request.group_id.as_deref(), LaunchOrigin::Terminal)?;
        spawn_agent_pane(&state, request)
    })
    .await
    .map_err(|err| format!("agent_spawn task failed: {err}"))?
}

#[tauri::command]
async fn spawn_claude(
    state: tauri::State<'_, AppState>,
    request: SpawnClaudeRequest,
) -> Result<PaneInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let request = request.into_agent_request();
        validate_launch_workspace(&state, request.group_id.as_deref(), LaunchOrigin::Terminal)?;
        spawn_agent_pane(&state, request)
    })
    .await
    .map_err(|err| format!("spawn_claude task failed: {err}"))?
}

/// Forks the session in `pane_id` into a new tab and resumes it. `nest` places the
/// fork as a child of the source; otherwise it lands as a sibling immediately after
/// it. When `prompt` is set, it is submitted as the fork's launch message.
#[tauri::command]
async fn agent_fork(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    use_worktree: bool,
    nest: bool,
    prompt: Option<String>,
    anchor: Option<MessageAnchor>,
) -> Result<PaneInfo, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(group_id) = state.pane_group_id(&pane_id)? {
            validate_launch_workspace(&state, Some(&group_id), LaunchOrigin::Terminal)?;
        }
        fork_agent_pane(&state, &pane_id, use_worktree, nest, prompt, anchor)
    })
    .await
    .map_err(|err| format!("agent_fork task failed: {err}"))?
}

// pane_write and every agent turn-queue command below are `(async)` as a
// correctness requirement, not just latency: a submit holds the pane's send
// lock across native bridge calls that each hop to the main thread, so no
// synchronous (main-thread) command may ever contend for a send lock — see
// write_pane. This also keeps the 15ms submit-key delay off the main thread.
#[tauri::command(async)]
fn pane_write(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    data: String,
    paste: bool,
    submit: bool,
) -> Result<(), String> {
    write_pane(
        &state,
        PaneWriteOptions {
            pane_id,
            data,
            paste,
            submit,
        },
    )
}

/// Signals that the webview's listener for `pane_id` is live, flushing any
/// output the pane produced before the frontend was ready to receive it.
#[tauri::command(async)]
fn pane_attach(state: tauri::State<'_, AppState>, pane_id: String) -> Result<(), String> {
    attach_pane(&state, pane_id)
}

/// Marks the webview's qmux-event listener as live. Until this arrives (and
/// again after any page navigation clears it, see `on_page_load` below), the
/// native shortcut classifiers decline to consume chords: their events would
/// be dropped by Tauri with nobody subscribed, turning consumed keystrokes
/// into nothing.
#[tauri::command]
fn mark_events_listener_ready() {
    native_terminal::set_events_listener_ready(true);
}

/// User-invoked escape hatch (pane context menu) for a terminal a crashed or
/// killed TUI left in a broken state: clears latched modes — kitty keyboard
/// flags, mouse/focus reporting, the alternate screen — without touching the
/// running process or the visible content. Async like the other pane commands
/// that take the scrollback I/O lock.
#[tauri::command(async)]
fn pane_reset_terminal_modes(
    state: tauri::State<'_, AppState>,
    pane_id: String,
) -> Result<(), String> {
    pty::reset_pane_terminal_modes(&state, &pane_id)
}

#[tauri::command]
fn pane_resize(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    resize_pane(&state, pane_id, cols, rows)
}

#[tauri::command]
async fn pane_activity(
    state: tauri::State<'_, AppState>,
    pane_id: String,
) -> Result<PaneActivity, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || inspect_pane_activity(&state, pane_id))
        .await
        .map_err(|err| format!("pane_activity task failed: {err}"))?
}

#[tauri::command]
async fn pane_kill(state: tauri::State<'_, AppState>, pane_id: String) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.close_pane_for_user(&pane_id))
        .await
        .map_err(|err| format!("pane_kill task failed: {err}"))?
}

/// Records that `pane_id` is now the focused pane. Fired by the frontend whenever
/// the active pane changes; feeds the recency signal that `group_spawn_cwd` uses to
/// pick a spawn directory. Best-effort and cheap (in-memory stamp, no persist).
#[tauri::command]
fn pane_activate(state: tauri::State<'_, AppState>, pane_id: String) {
    state.touch_pane_active(&pane_id);
}

#[tauri::command]
async fn pane_restore_last_closed(
    state: tauri::State<'_, AppState>,
) -> Result<Option<PaneInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || recovery::restore_last_closed_pane(&state))
        .await
        .map_err(|err| format!("pane_restore_last_closed task failed: {err}"))?
}

#[tauri::command]
fn pane_rename(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    title: String,
) -> Result<PaneInfo, String> {
    state.rename_pane(&pane_id, title)
}

#[tauri::command]
fn pane_reorder(
    state: tauri::State<'_, AppState>,
    pane_ids: Vec<String>,
) -> Result<Vec<PaneInfo>, String> {
    state.reorder_panes(pane_ids)
}

#[tauri::command]
fn pane_set_layout(
    state: tauri::State<'_, AppState>,
    items: Vec<PaneLayoutEntry>,
) -> Result<Vec<PaneInfo>, String> {
    state.set_pane_layout(items)
}

#[tauri::command]
fn pane_move_to_group(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    target_group_id: String,
    items: Vec<PaneLayoutEntry>,
) -> Result<Vec<PaneInfo>, String> {
    state.move_pane_to_group(&pane_id, &target_group_id, items)
}

#[tauri::command]
fn pane_place_after(
    state: tauri::State<'_, AppState>,
    pane_id: String,
    sibling_pane_id: String,
) -> Result<Vec<PaneInfo>, String> {
    state.place_pane_after(&pane_id, &sibling_pane_id)
}

#[tauri::command]
fn pane_splits_get(state: tauri::State<'_, AppState>) -> Result<Vec<PaneSplitInfo>, String> {
    state.pane_splits()
}

#[tauri::command]
fn pane_splits_set(
    state: tauri::State<'_, AppState>,
    splits: Vec<PaneSplitInfo>,
) -> Result<Vec<PaneSplitInfo>, String> {
    state.set_pane_splits(splits)
}

#[tauri::command(async)]
fn agent_submit_turn(
    state: tauri::State<'_, AppState>,
    request: SubmitAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    submit_agent_turn(&state, request)
}

#[tauri::command(async)]
fn agent_queue_wait_turn(
    state: tauri::State<'_, AppState>,
    request: QueueWaitAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    queue_wait_agent_turn(&state, request)
}

#[tauri::command(async)]
fn agent_queue_delivery_turn(
    state: tauri::State<'_, AppState>,
    request: QueueDeliveryAgentTurnRequest,
) -> Result<SubmitAgentTurnResult, String> {
    queue_delivery_agent_turn(&state, request)
}

#[tauri::command(async)]
fn agent_remove_queued_turn(
    state: tauri::State<'_, AppState>,
    request: RemoveQueuedAgentTurnRequest,
) -> Result<RemoveQueuedAgentTurnResult, String> {
    remove_queued_agent_turn(&state, request)
}

#[tauri::command(async)]
fn agent_reorder_queued_turn(
    state: tauri::State<'_, AppState>,
    request: ReorderQueuedAgentTurnRequest,
) -> Result<ReorderQueuedAgentTurnResult, String> {
    reorder_queued_agent_turn(&state, request)
}

#[tauri::command(async)]
fn agent_move_queued_turn(
    state: tauri::State<'_, AppState>,
    request: MoveQueuedAgentTurnRequest,
) -> Result<MoveQueuedAgentTurnResult, String> {
    move_queued_agent_turn(&state, request)
}

#[tauri::command(async)]
fn list_global_drafts(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<state::GlobalDraft>, String> {
    state.global_drafts()
}

#[tauri::command(async)]
fn create_global_draft(
    state: tauri::State<'_, AppState>,
    text: String,
) -> Result<state::GlobalDraft, String> {
    state.create_global_draft(text)
}

#[tauri::command(async)]
fn update_global_draft(
    state: tauri::State<'_, AppState>,
    draft_id: String,
    text: String,
) -> Result<state::GlobalDraft, String> {
    state.update_global_draft(&draft_id, text)
}

#[tauri::command(async)]
fn delete_global_draft(
    state: tauri::State<'_, AppState>,
    draft_id: String,
) -> Result<Vec<state::GlobalDraft>, String> {
    state.delete_global_draft(&draft_id)
}

#[tauri::command(async)]
fn assign_global_draft(
    state: tauri::State<'_, AppState>,
    request: AssignGlobalDraftRequest,
) -> Result<AssignGlobalDraftResult, String> {
    turn_queue::assign_global_draft(&state, request)
}

#[tauri::command(async)]
fn agent_send_next_queued_turn(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    send_next_queued_agent_turn(&state, &agent_id)
}

#[tauri::command(async)]
fn agent_set_queued_turn_pause(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    index: usize,
    pause_after: bool,
    expected_data: Option<String>,
    expected_id: Option<String>,
) -> Result<Vec<QueuedTurn>, String> {
    let queued_turns = state.set_queued_turn_pause(
        &agent_id,
        index,
        pause_after,
        expected_data.as_deref(),
        expected_id.as_deref(),
    )?;
    if let Some(agent) = state.agent(&agent_id)? {
        state.emit(events::QmuxEvent::new(
            "agent.queued_turn_reordered",
            agent.pane_id.clone(),
            Some(agent.id),
            serde_json::json!({
                "pendingTurns": queued_turns.len(),
                "queuedTurns": queued_turns.clone(),
            }),
        ));
    }
    Ok(queued_turns)
}

#[tauri::command(async)]
fn agent_unpause(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    unpause_agent(&state, &agent_id)
}

#[tauri::command(async)]
fn agent_set_typing(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    typing: bool,
) -> Result<SendNextQueuedAgentTurnResult, String> {
    set_agent_typing(&state, &agent_id, typing)
}

#[tauri::command]
fn agent_set_draft(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    draft: String,
) -> Result<(), String> {
    state.set_agent_draft(&agent_id, draft)
}

#[tauri::command]
fn agent_get_draft(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<Option<String>, String> {
    state.agent_draft(&agent_id)
}

// Async like the turn-queue commands above, and for the same send-lock
// invariant: acknowledging releases waiters and clearing a working status
// routes through advance_after_idle — both can drain a queued turn into a
// pane, which takes its send lock.
#[tauri::command(async)]
fn agent_acknowledge(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    include_failed: bool,
) -> Result<AgentInfo, String> {
    acknowledge_agent(&state, &agent_id, include_failed)
}

#[tauri::command(async)]
fn agent_clear_working_status(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<AgentInfo, String> {
    clear_agent_working_status(&state, &agent_id)
}

#[tauri::command]
async fn worktree_status(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<WorktreeStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || agent_worktree_status(&state, &agent_id))
        .await
        .map_err(|err| format!("worktree_status task failed: {err}"))?
}

#[tauri::command]
async fn worktree_signature(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || agent_worktree_signature(&state, &agent_id))
        .await
        .map_err(|err| format!("worktree_signature task failed: {err}"))?
}

#[tauri::command]
async fn worktree_remove(
    state: tauri::State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || remove_agent_worktree(&state, &agent_id))
        .await
        .map_err(|err| format!("worktree_remove task failed: {err}"))?
}

#[tauri::command]
async fn worktree_close_pane(
    state: tauri::State<'_, AppState>,
    agent_id: String,
    delete_worktree: bool,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        close_worktree_pane(&state, &agent_id, delete_worktree)
    })
    .await
    .map_err(|err| format!("worktree_close_pane task failed: {err}"))?
}

#[tauri::command]
fn app_confirm_exit(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state.mark_exit_confirmed();
    app.exit(0);
    Ok(())
}

/// Whether the frontend has reported its first meaningful paint. Read by the
/// startup watchdog so it only force-shows the window when the frontend never
/// booted — not when the user has already seen the window and hidden it again.
static WINDOW_READY_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The window starts hidden (`visible: false` in tauri.conf.json) so startup
/// never shows a blank translucent shell while the session restores and the
/// webview boots. The frontend calls this once its boot snapshot is applied —
/// the first paint the user sees is the restored session. A watchdog in setup
/// shows the window anyway if the frontend never reports in.
#[tauri::command]
fn app_window_ready(app: tauri::AppHandle) -> Result<(), String> {
    WINDOW_READY_REPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
    show_main_window(&app);
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Arms or releases the macOS wake lock. The frontend calls this whenever its
/// "prevent sleep" setting or the set of running agents changes.
#[tauri::command(async)]
fn app_set_prevent_sleep(guard: tauri::State<'_, SleepGuard>, active: bool) -> Result<(), String> {
    guard.set_active(active)
}

#[tauri::command(async)]
async fn generate_foundation_tab_title(message: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        title_generation::generate_foundation_title(&message)
    })
    .await
    .map_err(|err| format!("Apple Foundation Models task failed: {err}"))?
}

pub(crate) fn ensure_rustls_crypto_provider() -> Result<(), String> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    rustls::crypto::CryptoProvider::get_default()
        .map(|_| ())
        .ok_or_else(|| "failed to install the rustls ring crypto provider".to_string())
}

fn main() {
    ensure_rustls_crypto_provider().unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });
    match cli::run_cli_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }

    let config = QmuxConfig::load().unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });
    let state = AppState::new(config);

    let exit_state = state.clone();

    tauri::Builder::default()
        // Registered first so a duplicate launch exits before setup() can steal the
        // control socket and respawn the persisted session alongside the running
        // instance. Instances are deduped per app identifier and user session; the
        // second launch hands off to this callback in the surviving process, which
        // just surfaces the existing window. (CLI subcommands returned above and
        // never get here, so `qmux open` etc. are unaffected.)
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(handle_global_shortcut)
                .build(),
        )
        .on_window_event({
            let state = state.clone();
            move |window, event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if window.label() == "global-task-launcher" {
                        api.prevent_close();
                        let _ = window.hide();
                    } else if state.should_confirm_exit() {
                        api.prevent_close();
                        state.request_exit_confirmation();
                    }
                }
            }
        })
        .on_menu_event(handle_app_menu_event)
        // A page navigation (reload, dev HMR full-reload) tears down the old
        // document's qmux-event listener; clear the readiness flag so native
        // shortcut classifiers stop consuming chords until the new document
        // re-subscribes and calls mark_events_listener_ready again.
        .on_page_load(|webview, payload| {
            // The quick-launch webview has its own event subscription. Loading
            // or reloading it must not revoke the main document's native
            // terminal shortcut readiness.
            if webview.label() == "main"
                && payload.event() == tauri::webview::PageLoadEvent::Started
            {
                native_terminal::set_events_listener_ready(false);
            }
        })
        .setup({
            let state = state.clone();
            move |app| {
                // First thing, so the login-shell PATH probe (up to seconds under
                // heavy shell profiles) overlaps the rest of startup instead of
                // stalling the first recovered pane's spawn at the end of it.
                launch_path::warm_login_shell_path();
                state
                    .attach_app(app.handle().clone())
                    .map_err(std::io::Error::other)?;
                // Freeze diagnostics: start the heartbeat watchdog and mark the
                // launch in the durable log, so records across restarts are
                // separable when reading <workspace_root>/.qmux/diagnostics.log.
                diagnostics::spawn_watchdog(state.clone());
                state.diagnostics().record(
                    "backend",
                    "app.started",
                    "qmux backend started",
                    serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
                );
                #[cfg(target_os = "macos")]
                if !native_terminal::available() {
                    return Err(std::io::Error::other(
                        "the native Ghostty terminal bridge failed to initialize",
                    )
                    .into());
                }
                #[cfg(target_os = "macos")]
                if let Some(window) = app.get_webview_window("main") {
                    native_terminal::initialize(window.ns_view()?, state.clone())
                        .map_err(std::io::Error::other)?;
                }
                // Best-effort: if the menu tweak fails, ⌘W keeps its default
                // (window-closing) behavior and ⌘Q its instant-quit behavior rather
                // than aborting startup.
                if let Err(err) = customize_app_menu(app) {
                    eprintln!("qmux: failed to customize app menu: {err}");
                }
                if let Err(err) = menu_bar::init(app.handle()) {
                    eprintln!("qmux: failed to initialize menu bar icon: {err}");
                }
                app.manage(show_hide_shortcut::ShowHideShortcutState::default());
                show_hide_shortcut::init(app.handle(), &state.config().workspace_root);
                app.manage(global_task_launcher::GlobalTaskLauncherState::default());
                global_task_launcher::create_window(app)?;
                global_task_launcher::init(app.handle(), &state.config().workspace_root);
                // On macOS, give the window an NSVisualEffectView so the sidebar can
                // read as a native, translucent source list (Finder/Mail/Xcode). The
                // frontend paints the content panes opaque and leaves the sidebar
                // column transparent, so the material only shows through there.
                #[cfg(target_os = "macos")]
                {
                    use window_vibrancy::{NSVisualEffectMaterial, apply_vibrancy};
                    if let Some(window) = app.get_webview_window("main")
                        && let Err(err) =
                            apply_vibrancy(&window, NSVisualEffectMaterial::Sidebar, None, None)
                    {
                        eprintln!("qmux: failed to apply window vibrancy: {err}");
                    }
                }
                start_control_socket(state.clone()).map_err(std::io::Error::other)?;
                // Loopback static server for the browser overlay. Best-effort: if it
                // can't bind, the app still runs (file:// opens just won't work).
                match file_server::start_file_server(state.clone()) {
                    Ok(info) => state.set_file_server(info.port),
                    Err(err) => eprintln!("qmux: failed to start file server: {err}"),
                }
                // Refuse to continue if the saved session exists but can't be read:
                // starting empty here would let the first save overwrite it with
                // nothing and no backup. Abort loudly (terminal + GUI) so a relaunch
                // after fixing the transient cause restores the session intact.
                if let Err(err) = state.preflight_persisted_state() {
                    eprintln!("\nqmux: {err}\n");
                    notify_fatal_startup(&err);
                    std::process::exit(1);
                }
                // Sweep scratch files stranded by earlier processes that were killed
                // mid-save (most commonly the final persist racing process exit).
                // Backgrounded: it scans the state dir and only ever removes files
                // whose unique pid-tagged names no live save can be using, so it
                // doesn't need to gate window creation.
                {
                    let workspace_root = state.config().workspace_root.clone();
                    std::thread::spawn(move || {
                        persistence::remove_stale_tmp_files(&workspace_root);
                    });
                }
                // Restore persisted groups/agents/queues, then respawn recoverable
                // panes into fresh PTYs before the command handlers go live so the
                // webview's first list_panes() already sees the recovered session.
                let recovered_panes = state.restore_session();
                workspace::reconcile_imported_research_archives(&state);
                // Recovery fell back to an empty session (state discarded to a .bak)
                // or dropped entries: say so in a dialog, since a Finder launch never
                // shows stderr and silent session loss looks like qmux ate the tabs.
                if let Some(warning) = state.take_recovery_warning() {
                    notify_startup_warning(app.handle(), &warning);
                }
                recovery::respawn_session(&state, recovered_panes);
                // Re-level persisted nesting now that we know which panes actually
                // came back (exited panes are not respawned).
                state.normalize_pane_layout();
                // Now that the surviving panes are known, sweep scrollback logs
                // and trim scratch files no live pane owns — orphans a kill or an
                // unrecovered pane left on disk holding raw terminal output.
                // Backgrounded like the state-dir scratch sweep; the live pane-id
                // set is captured up front and new panes self-heal a racing delete.
                {
                    let workspace_root = state.config().workspace_root.clone();
                    let live_pane_ids: std::collections::HashSet<String> = state
                        .list_panes()
                        .map(|panes| panes.into_iter().map(|pane| pane.id).collect())
                        .unwrap_or_default();
                    std::thread::spawn(move || {
                        scrollback::remove_orphaned_scrollback(&workspace_root, &live_pane_ids);
                    });
                }
                app.manage(state.clone());
                app.manage(SleepGuard::default());
                // Watchdog for the hidden-at-boot window: if the frontend fails
                // to boot (dev server down, bundle error) it can never call
                // app_window_ready, and an invisible app that must be force-quit
                // is worse than a blank window. Show it after a grace period.
                {
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(10));
                        if !WINDOW_READY_REPORTED.load(std::sync::atomic::Ordering::Relaxed) {
                            let app_handle_on_main = app_handle.clone();
                            let _ = app_handle.run_on_main_thread(move || {
                                show_main_window(&app_handle_on_main);
                            });
                        }
                    });
                }
                updater::check_on_startup(app.handle());
                Ok(())
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_window_ready,
            get_runtime_config,
            launcher_adapter_preference_get,
            launcher_adapter_preference_set,
            openrouter_key_get,
            openrouter_key_set,
            openrouter_chat_completion,
            publishing::publishing_auth_status,
            publishing::publishing_auth_begin,
            publishing::publishing_auth_poll,
            publishing::publishing_auth_disconnect,
            publishing::publishing_publish,
            publishing::publishing_sync,
            publishing::publishing_list,
            publishing::publishing_list_proposals,
            publishing::publishing_resolve_proposal,
            active_tab_get,
            active_tab_set,
            open_external_url,
            prompt_library_list,
            prompt_library_save,
            prompt_library_delete,
            prompt_library_reveal,
            list_claude_skills,
            list_panes,
            list_groups,
            list_research_workspaces,
            ensure_default_research_workspace_command,
            research_workspace_create_pick,
            research_workspace_rename,
            research_workspace_move_pick,
            research_workspace_remove,
            research_workspace_reveal,
            list_agents,
            list_shell_agent_jobs,
            list_recent_sessions,
            agent_branches,
            recent_session_resume,
            list_turns,
            list_thread_graphs,
            get_thread_graph,
            list_research_trees,
            reorder_research_trees,
            list_research_folders,
            set_research_folders,
            list_research_activity,
            get_research_tree,
            create_research_tree,
            create_research_document,
            export_pane_to_research,
            update_research_document,
            read_markdown_document_file,
            encyclopedia_status,
            encyclopedia_set_settings,
            encyclopedia_update,
            encyclopedia_cancel,
            encyclopedia_read_page,
            read_transcript_image,
            get_research_node_content,
            fork_research_node,
            cancel_research_node,
            rename_research_tree,
            rename_research_node,
            create_research_highlight,
            remove_research_highlight,
            remove_research_highlights,
            mark_research_tree_viewed,
            archive_research_tree,
            restore_research_tree,
            remove_research_tree,
            remove_research_branch,
            list_agent_turn_queue,
            list_agent_transcripts,
            set_agent_transcript,
            group_create,
            group_remove,
            group_rename,
            group_reorder,
            group_set_collapsed,
            group_create_pick,
            group_pick_dir,
            spawn_shell,
            use_login_shell_get,
            use_login_shell_set,
            research_launch_instruction_get,
            research_launch_instruction_set,
            worktree_location_get,
            worktree_location_set,
            agent_spawn,
            spawn_claude,
            agent_fork,
            pane_write,
            pane_attach,
            pane_reset_terminal_modes,
            mark_events_listener_ready,
            pane_resize,
            pane_activity,
            pane_kill,
            pane_activate,
            pane_restore_last_closed,
            pane_rename,
            pane_reorder,
            pane_set_layout,
            pane_move_to_group,
            pane_place_after,
            pane_splits_get,
            pane_splits_set,
            native_terminal_set_keyboard_owner,
            native_terminal_set_layout,
            native_terminal_set_stage_backstop,
            native_terminal_set_web_pointer_claimed,
            native_terminal_set_web_overlay_region,
            native_terminal_set_iframe_shortcut_fallback,
            native_terminal_focus,
            native_terminal_action,
            native_terminal_paste_approved_text,
            native_terminal_update_settings,
            native_terminal_seed_settings,
            native_terminal_theme_catalog,
            agent_submit_turn,
            agent_queue_wait_turn,
            agent_queue_delivery_turn,
            agent_remove_queued_turn,
            agent_reorder_queued_turn,
            agent_move_queued_turn,
            list_global_drafts,
            create_global_draft,
            update_global_draft,
            delete_global_draft,
            assign_global_draft,
            agent_send_next_queued_turn,
            agent_set_queued_turn_pause,
            agent_unpause,
            agent_set_typing,
            agent_set_draft,
            agent_get_draft,
            agent_acknowledge,
            agent_clear_working_status,
            worktree_status,
            worktree_signature,
            worktree_remove,
            worktree_close_pane,
            app_confirm_exit,
            app_set_prevent_sleep,
            generate_foundation_tab_title,
            menu_bar_update,
            show_hide_shortcut_get,
            show_hide_shortcut_set,
            show_hide_shortcut_capture_set,
            global_task_launcher::global_task_launcher_hotkey_get,
            global_task_launcher::global_task_launcher_hotkey_set,
            global_task_launcher::global_task_launcher_open,
            global_task_launcher::global_task_launcher_dismiss,
            diagnostics::diagnostics_record_batch,
            diagnostics::diagnostics_heartbeat,
            diagnostics::diagnostics_snapshot,
            diagnostics::diagnostics_log_path,
        ])
        .build(tauri::generate_context!())
        .expect("error while building qmux")
        .run(move |_app_handle, event| match event {
            tauri::RunEvent::ExitRequested { api, code, .. }
                if code != Some(tauri::RESTART_EXIT_CODE) && exit_state.should_confirm_exit() =>
            {
                api.prevent_exit();
                exit_state.request_exit_confirmation();
            }
            // The process is really terminating now (exit confirmed, or nothing to
            // confirm). Take down every pane's process tree so agent-spawned
            // descendants don't survive as orphans past quit.
            tauri::RunEvent::Exit => {
                // Freeze the on-disk session before touching the panes: killing them
                // makes every reader thread see PTY EOF and run the natural-exit
                // remove_pane path, and any of those persists that win the race with
                // process death would save the session with its tabs deleted.
                exit_state.finalize_persistence_for_exit();
                pty::kill_all_panes(&exit_state);
                native_terminal::shutdown();
                // Reclaim the control socket on a clean exit rather than leaving the
                // file for the next launch's stale-socket cleanup — but only while the
                // path still points at the socket this process bound. If another
                // instance (e.g. a differently-configured build sharing the socket
                // path) has re-bound it since, deleting the file would sever every
                // `qmux` CLI caller from that live instance.
                if exit_state.owns_control_socket() {
                    let _ = std::fs::remove_file(&exit_state.config().socket_path);
                }
            }
            _ => {}
        });
}
