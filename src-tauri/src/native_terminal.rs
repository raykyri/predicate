use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::events::QmuxEvent;
use crate::state::AppState;

static APP_STATE: OnceLock<Mutex<Option<AppState>>> = OnceLock::new();
static REPLAYING_PANES: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Main-thread delegate reports that need the model lock or the filesystem
/// (OSC title, cwd). They arrive on the AppKit main thread, and applying them
/// there parks the UI behind whatever backend thread currently holds the
/// model lock (persist snapshot, research launch/retire churn) — the same
/// stall `qmux_native_terminal_did_close`'s worker handoff already avoids. A
/// single draining worker preserves per-pane ordering and collapses bursts.
enum DeferredPaneReport {
    Title { pane_id: String, title: String },
    Cwd { pane_id: String, cwd: String },
}

impl DeferredPaneReport {
    fn pane_id(&self) -> &str {
        match self {
            DeferredPaneReport::Title { pane_id, .. } | DeferredPaneReport::Cwd { pane_id, .. } => {
                pane_id
            }
        }
    }
}

static DEFERRED_REPORT_TX: std::sync::LazyLock<std::sync::mpsc::Sender<DeferredPaneReport>> =
    std::sync::LazyLock::new(|| {
        let (tx, rx) = std::sync::mpsc::channel::<DeferredPaneReport>();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                // Drain the burst before applying: a later report for the same
                // pane supersedes an earlier one (last title/cwd wins), so a
                // title-spamming program can't build an unbounded backlog.
                let mut batch = vec![first];
                while let Ok(report) = rx.try_recv() {
                    batch.push(report);
                }
                for (index, report) in batch.iter().enumerate() {
                    let superseded = batch[index + 1..]
                        .iter()
                        .any(|later| later.pane_id() == report.pane_id());
                    if !superseded {
                        apply_deferred_pane_report(report);
                    }
                }
            }
        });
        tx
    });

fn apply_deferred_pane_report(report: &DeferredPaneReport) {
    with_app_state(|state| match report {
        DeferredPaneReport::Title { pane_id, title } => {
            let event_title = match state.update_last_osc_title(pane_id, title) {
                Ok(title) => title.unwrap_or_default(),
                Err(err) => {
                    eprintln!("qmux: failed to record native title for pane {pane_id}: {err}");
                    // Persistence is best-effort. Preserve the pre-existing live
                    // behavior even if the model is temporarily unavailable; the
                    // frontend still applies its own sanitization.
                    title.clone()
                }
            };
            state.emit(QmuxEvent::new(
                "terminal.title_changed",
                Some(pane_id.clone()),
                None,
                serde_json::json!({ "title": event_title }),
            ));
        }
        DeferredPaneReport::Cwd { pane_id, cwd } => {
            if let Err(err) = state.update_pane_cwd(pane_id, cwd.clone()) {
                eprintln!("qmux: rejected native cwd update for pane {pane_id}: {err}");
            }
        }
    });
}
/// Whether the webview's qmux-event listener is live. The native shortcut
/// classifiers report a chord as "handled" — which makes Swift consume its
/// keyDown and keyUp — purely by emitting an event; while no listener exists
/// (startup, a page reload) that emit is dropped by Tauri and the chord would
/// be consumed and then do nothing. Set by the frontend once its subscription
/// resolves, cleared whenever the webview starts loading a page.
static EVENTS_LISTENER_READY: AtomicBool = AtomicBool::new(false);

pub fn set_events_listener_ready(ready: bool) {
    EVENTS_LISTENER_READY.store(ready, Ordering::Release);
}

fn events_listener_ready() -> bool {
    EVENTS_LISTENER_READY.load(Ordering::Acquire)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTerminalLayout {
    pub pane_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
    pub accepts_pointer_input: bool,
    /// Whether a pointer gesture may optimistically grant this pane the
    /// keyboard before React confirms the desired owner. False when the keyboard denial
    /// is hard policy (read-only research panes, blocked input) rather than a
    /// transient focus state.
    pub accepts_keyboard_claim: bool,
    pub defer_geometry: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTerminalKeyboardOwnerUpdate {
    pub pane_id: Option<String>,
    pub revision: u64,
}

/// A DOM rectangle that keeps pointer events routed to the webview even where
/// it overlaps a native terminal surface (floating controls drawn over the
/// terminal). `visible: false` removes the region.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeWebOverlayRegion {
    pub region_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub visible: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTerminalSettings {
    pub pane_id: String,
    pub font_size: f64,
    pub font_family: String,
    pub letter_spacing: f64,
    pub line_height: f64,
    pub cursor_blink: bool,
    pub cursor_style: String,
    pub scrollback_rows: u32,
    pub scroll_on_user_input: bool,
    pub scroll_sensitivity: f64,
    pub copy_on_select: bool,
    pub selection_clear_on_copy: bool,
    pub theme_name: String,
}

/// A settings snapshot not bound to any pane. Seeded by the frontend at
/// startup and on every settings change so Swift can create a new pane's
/// Ghostty surface immediately at creation time instead of holding it back
/// until that pane's first per-pane settings update arrives.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTerminalSeedSettings {
    pub font_size: f64,
    pub font_family: String,
    pub letter_spacing: f64,
    pub line_height: f64,
    pub cursor_blink: bool,
    pub cursor_style: String,
    pub scrollback_rows: u32,
    pub scroll_on_user_input: bool,
    pub scroll_sensitivity: f64,
    pub copy_on_select: bool,
    pub selection_clear_on_copy: bool,
    pub theme_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppShortcutCommand {
    FontZoomIn,
    FontZoomOut,
    FontZoomReset,
    FocusTab(u8),
    HomeOrCycleAdapter,
    FocusHome,
    FocusResearchMode,
    ToggleSidebarMode,
    CyclePaneTab(i8),
    CycleAllTab(i8),
    MoveSidebarItem(i8),
    OpenSettings,
    OpenCommandPalette,
    FocusFollowups,
    OpenFolderMenu,
    ToggleTranscriptOrBrowser,
    SplitPaneBelow,
    RestoreClosedPane,
    ClosePane,
    NewGroup,
    NewPane,
}

impl AppShortcutCommand {
    fn event_fields(self) -> (&'static str, Option<u8>) {
        match self {
            Self::FontZoomIn => ("fontZoomIn", None),
            Self::FontZoomOut => ("fontZoomOut", None),
            Self::FontZoomReset => ("fontZoomReset", None),
            Self::FocusTab(index) => ("focusTab", Some(index)),
            Self::HomeOrCycleAdapter => ("homeOrCycleAdapter", None),
            Self::FocusHome => ("focusHome", None),
            Self::FocusResearchMode => ("focusResearchMode", None),
            Self::ToggleSidebarMode => ("toggleSidebarMode", None),
            Self::CyclePaneTab(-1) => ("cyclePaneTabPrevious", None),
            Self::CyclePaneTab(_) => ("cyclePaneTabNext", None),
            Self::CycleAllTab(-1) => ("cycleAllTabPrevious", None),
            Self::CycleAllTab(_) => ("cycleAllTabNext", None),
            Self::MoveSidebarItem(-1) => ("moveSidebarItemUp", None),
            Self::MoveSidebarItem(_) => ("moveSidebarItemDown", None),
            Self::OpenSettings => ("openSettings", None),
            Self::OpenCommandPalette => ("openCommandPalette", None),
            Self::FocusFollowups => ("focusFollowups", None),
            Self::OpenFolderMenu => ("openFolderMenu", None),
            Self::ToggleTranscriptOrBrowser => ("toggleTranscriptOrBrowser", None),
            Self::SplitPaneBelow => ("splitPaneBelow", None),
            Self::RestoreClosedPane => ("restoreClosedPane", None),
            Self::ClosePane => ("closePane", None),
            Self::NewGroup => ("newGroup", None),
            Self::NewPane => ("newPane", None),
        }
    }
}

fn classify_app_shortcut(
    key: &str,
    shift: bool,
    control: bool,
    option: bool,
    command: bool,
) -> Option<AppShortcutCommand> {
    let normalized_key = key.to_lowercase();
    let key = match normalized_key.as_str() {
        "{" => "[",
        "}" => "]",
        other => other,
    };
    let one_primary_modifier = command != control;

    if command && !control && option && !shift && (key == "arrowup" || key == "arrowdown") {
        return Some(AppShortcutCommand::MoveSidebarItem(if key == "arrowup" {
            -1
        } else {
            1
        }));
    }

    if command && !control && !option {
        if key == "+" || key == "=" {
            return Some(AppShortcutCommand::FontZoomIn);
        }
        if key == "-" && !shift {
            return Some(AppShortcutCommand::FontZoomOut);
        }
        if key == "0" && !shift {
            return Some(AppShortcutCommand::FontZoomReset);
        }
    }

    if one_primary_modifier && !option && !shift && key.len() == 1 {
        let digit = key.as_bytes()[0];
        if (b'1'..=b'9').contains(&digit) {
            return Some(AppShortcutCommand::FocusTab(digit - b'1'));
        }
    }
    if command && !control && !option && !shift && key == "n" {
        return Some(AppShortcutCommand::HomeOrCycleAdapter);
    }
    if command && !control && !option && shift && key == "h" {
        return Some(AppShortcutCommand::FocusHome);
    }
    if command && !control && !option && shift && key == "r" {
        return Some(AppShortcutCommand::FocusResearchMode);
    }
    if command && !control && !option && !shift && key == "`" {
        return Some(AppShortcutCommand::ToggleSidebarMode);
    }
    if !command && control && !option && key == "tab" {
        return Some(AppShortcutCommand::CyclePaneTab(if shift { -1 } else { 1 }));
    }
    if command && !control && !option && shift && (key == "[" || key == "]") {
        return Some(AppShortcutCommand::CycleAllTab(if key == "[" {
            -1
        } else {
            1
        }));
    }
    if one_primary_modifier && !option && !shift && key == "," {
        return Some(AppShortcutCommand::OpenSettings);
    }
    // ⌘K is deliberately NOT classified here: with a terminal focused it stays
    // native (clear screen). The command palette binds it only for web targets,
    // in appShortcuts.ts.
    if one_primary_modifier && !option && shift && key == "e" {
        return Some(AppShortcutCommand::ToggleTranscriptOrBrowser);
    }
    // Research-surface commands, claimed even for a focused terminal so they
    // keep working on research-scoped live terminals. Ghostty binds neither
    // chord, so a terminal loses nothing (its AppKit layer swallows unbound
    // Command chords anyway); the frontend no-ops them outside research mode.
    if command && !control && !option && !shift && key == "j" {
        return Some(AppShortcutCommand::FocusFollowups);
    }
    if command && !control && !option && !shift && key == "o" {
        return Some(AppShortcutCommand::OpenFolderMenu);
    }
    if command && !control && !option && key == "d" {
        return Some(AppShortcutCommand::SplitPaneBelow);
    }
    if command && !control && !option && shift && key == "t" {
        return Some(AppShortcutCommand::RestoreClosedPane);
    }
    if command && !control && !option && shift && key == "n" {
        return Some(AppShortcutCommand::NewGroup);
    }
    if command && !control && !option && !shift && key == "w" {
        return Some(AppShortcutCommand::ClosePane);
    }
    if command && !control && !option && !shift && key == "t" {
        return Some(AppShortcutCommand::NewPane);
    }

    None
}

/// Native shortcut routing without a live terminal has web-target semantics.
/// In particular, Cmd-K opens qmux's palette there, while the terminal
/// classifier above must leave the same chord to Ghostty's clear-screen binding.
fn classify_web_app_shortcut(
    key: &str,
    shift: bool,
    control: bool,
    option: bool,
    command: bool,
) -> Option<AppShortcutCommand> {
    if command && !control && !option && !shift && key.eq_ignore_ascii_case("k") {
        return Some(AppShortcutCommand::OpenCommandPalette);
    }
    classify_app_shortcut(key, shift, control, option, command)
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod imp {
    use super::APP_STATE;
    use super::NativeTerminalKeyboardOwnerUpdate;
    use super::NativeTerminalLayout;
    use super::NativeTerminalSeedSettings;
    use super::NativeTerminalSettings;
    use super::NativeWebOverlayRegion;
    use crate::state::AppState;
    use std::ffi::{CString, c_char, c_void};
    use std::sync::Mutex;

    unsafe extern "C" {
        fn qmux_native_terminal_bridge_available() -> i32;
        fn qmux_native_terminal_should_claim_web_app_shortcut(
            has_terminal_keyboard_owner: i32,
            responder_state: i32,
            iframe_fallback_eligible: i32,
        ) -> i32;
        fn qmux_native_terminal_initialize(native_view: *mut c_void) -> i32;
        fn qmux_native_terminal_create_host_managed(
            pane_id: *const c_char,
            working_directory: *const c_char,
        ) -> i32;
        fn qmux_native_terminal_receive(
            pane_id: *const c_char,
            bytes: *const u8,
            bytes_len: usize,
        ) -> i32;
        fn qmux_native_terminal_is_ready_for_replay(pane_id: *const c_char) -> i32;
        fn qmux_native_terminal_remove(pane_id: *const c_char);
        fn qmux_native_terminal_set_stage_backstop(x: f64, y: f64, width: f64, height: f64) -> i32;
        fn qmux_native_terminal_set_layout(
            pane_id: *const c_char,
            x: f64,
            y: f64,
            width: f64,
            height: f64,
            visible: i32,
            accepts_pointer_input: i32,
            accepts_keyboard_claim: i32,
            defer_geometry: i32,
        ) -> i32;
        fn qmux_native_terminal_set_keyboard_owner(pane_id: *const c_char, revision: u64) -> i32;
        fn qmux_native_terminal_set_web_pointer_claimed(claimed: i32) -> i32;
        fn qmux_native_terminal_set_web_overlay_region(
            region_id: *const c_char,
            x: f64,
            y: f64,
            width: f64,
            height: f64,
            visible: i32,
        ) -> i32;
        fn qmux_native_terminal_set_iframe_shortcut_fallback(active: i32) -> i32;
        fn qmux_native_terminal_focus(pane_id: *const c_char) -> i32;
        fn qmux_native_terminal_send_text(pane_id: *const c_char, text: *const c_char) -> i32;
        fn qmux_native_terminal_submit(pane_id: *const c_char) -> i32;
        fn qmux_native_terminal_paste_approved_text(
            pane_id: *const c_char,
            text: *const u8,
            text_len: usize,
        ) -> i32;
        fn qmux_native_terminal_action(pane_id: *const c_char, action: *const c_char) -> i32;
        fn qmux_native_terminal_update_settings(
            pane_id: *const c_char,
            font_size: f64,
            font_family: *const c_char,
            letter_spacing: f64,
            line_height: f64,
            cursor_blink: i32,
            cursor_style: *const c_char,
            scrollback_rows: u32,
            scroll_on_user_input: i32,
            scroll_sensitivity: f64,
            copy_on_select: i32,
            selection_clear_on_copy: i32,
            theme_name: *const c_char,
        ) -> i32;
        fn qmux_native_terminal_seed_settings(
            font_size: f64,
            font_family: *const c_char,
            letter_spacing: f64,
            line_height: f64,
            cursor_blink: i32,
            cursor_style: *const c_char,
            scrollback_rows: u32,
            scroll_on_user_input: i32,
            scroll_sensitivity: f64,
            copy_on_select: i32,
            selection_clear_on_copy: i32,
            theme_name: *const c_char,
        ) -> i32;
        fn qmux_native_terminal_theme_catalog() -> *const c_char;
        fn qmux_native_terminal_shutdown();
    }

    fn cstring(value: &str, label: &str) -> Result<CString, String> {
        CString::new(value).map_err(|_| format!("{label} contains an interior NUL byte"))
    }

    pub fn available() -> bool {
        // SAFETY: the function has no arguments or borrowed state and is linked
        // from the pinned QmuxNativeTerminal Swift package in build.rs.
        unsafe { qmux_native_terminal_bridge_available() == 1 }
    }

    pub fn should_claim_web_app_shortcut(
        has_terminal_keyboard_owner: bool,
        responder_state: i32,
        iframe_fallback_eligible: bool,
    ) -> bool {
        // SAFETY: all arguments are scalar values. Swift validates the
        // responder-state discriminant before exercising the pure routing
        // helper linked from the same package as the terminal bridge.
        unsafe {
            qmux_native_terminal_should_claim_web_app_shortcut(
                i32::from(has_terminal_keyboard_owner),
                responder_state,
                i32::from(iframe_fallback_eligible),
            ) == 1
        }
    }

    pub fn initialize(native_view: *mut c_void, state: AppState) -> Result<(), String> {
        if native_view.is_null() {
            return Err("Tauri returned a null native content view".to_string());
        }
        // SAFETY: Tauri owns this NSView for the duration of the application and
        // Swift retains only a weak reference to its window plus a child view.
        if unsafe { qmux_native_terminal_initialize(native_view) } == 1 {
            let state_slot = APP_STATE.get_or_init(|| Mutex::new(None));
            *state_slot
                .lock()
                .map_err(|_| "native terminal state lock poisoned".to_string())? = Some(state);
            Ok(())
        } else {
            Err("failed to attach the native terminal host beneath WKWebView".to_string())
        }
    }

    pub fn create_host_managed(
        pane_id: &str,
        working_directory: Option<&str>,
    ) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        let working_directory = working_directory
            .map(|value| cstring(value, "working directory"))
            .transpose()?;
        let working_directory_ptr = working_directory
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr());
        // SAFETY: Swift copies both strings synchronously before returning.
        if unsafe {
            qmux_native_terminal_create_host_managed(pane_id.as_ptr(), working_directory_ptr)
        } == 1
        {
            Ok(())
        } else {
            Err("failed to create the host-managed native terminal surface".to_string())
        }
    }

    pub fn receive(pane_id: &str, bytes: &[u8], replay: bool) -> Result<(), String> {
        let pane_id_c = cstring(pane_id, "pane id")?;
        if replay && let Ok(mut panes) = super::REPLAYING_PANES.lock() {
            panes.insert(pane_id.to_string());
        }
        // SAFETY: Swift copies the byte buffer synchronously before returning.
        let received = unsafe {
            qmux_native_terminal_receive(pane_id_c.as_ptr(), bytes.as_ptr(), bytes.len()) == 1
        };
        if replay && let Ok(mut panes) = super::REPLAYING_PANES.lock() {
            panes.remove(pane_id);
        }
        if received {
            Ok(())
        } else {
            Err("host-managed native terminal surface was not found".to_string())
        }
    }

    /// True once the pane's surface exists and has been fitted to a real
    /// (nonzero) frame, so replayed scrollback renders at the width the pane
    /// will actually keep instead of the zero-frame default grid.
    pub fn is_ready_for_replay(pane_id: &str) -> Result<bool, String> {
        let pane_id = cstring(pane_id, "pane id")?;
        // SAFETY: Swift copies the string synchronously before returning.
        Ok(unsafe { qmux_native_terminal_is_ready_for_replay(pane_id.as_ptr()) } == 1)
    }

    pub fn remove(pane_id: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        // SAFETY: Swift copies the string synchronously before returning.
        unsafe { qmux_native_terminal_remove(pane_id.as_ptr()) };
        Ok(())
    }

    /// Positions the opaque terminal-colored backstop under the webview's
    /// terminal stage. Pane surfaces chase their DOM rects asynchronously, so
    /// the backstop is what shows through transient gaps (pane spawn,
    /// Home→pane switches, split-resize lag) instead of the window's vibrancy
    /// material.
    pub fn set_stage_backstop(x: f64, y: f64, width: f64, height: f64) -> Result<(), String> {
        if !x.is_finite()
            || !y.is_finite()
            || !width.is_finite()
            || !height.is_finite()
            || width < 0.0
            || height < 0.0
        {
            return Err("native terminal backstop has invalid geometry".to_string());
        }
        // SAFETY: scalar arguments are copied synchronously on the main actor.
        if unsafe { qmux_native_terminal_set_stage_backstop(x, y, width, height) } == 1 {
            Ok(())
        } else {
            Err("native terminal host is not attached".to_string())
        }
    }

    pub fn set_layout(layout: NativeTerminalLayout) -> Result<(), String> {
        if !layout.x.is_finite()
            || !layout.y.is_finite()
            || !layout.width.is_finite()
            || !layout.height.is_finite()
            || layout.width < 0.0
            || layout.height < 0.0
        {
            return Err("native terminal layout contains invalid geometry".to_string());
        }
        let pane_id = cstring(&layout.pane_id, "pane id")?;
        // SAFETY: Swift copies the string and scalar layout synchronously.
        let success = unsafe {
            qmux_native_terminal_set_layout(
                pane_id.as_ptr(),
                layout.x,
                layout.y,
                layout.width,
                layout.height,
                i32::from(layout.visible),
                i32::from(layout.accepts_pointer_input),
                i32::from(layout.accepts_keyboard_claim),
                i32::from(layout.defer_geometry),
            )
        };
        if success == 1 {
            Ok(())
        } else {
            Err(format!(
                "native terminal pane {} was not found",
                layout.pane_id
            ))
        }
    }

    pub fn set_keyboard_owner(update: NativeTerminalKeyboardOwnerUpdate) -> Result<(), String> {
        let pane_id = update
            .pane_id
            .as_deref()
            .map(|pane_id| cstring(pane_id, "pane id"))
            .transpose()?;
        // SAFETY: Swift copies the optional string and scalar revision
        // synchronously on the main actor before returning.
        if unsafe {
            qmux_native_terminal_set_keyboard_owner(
                pane_id
                    .as_ref()
                    .map_or(std::ptr::null(), |pane_id| pane_id.as_ptr()),
                update.revision,
            )
        } == 1
        {
            Ok(())
        } else {
            Err("native terminal host rejected the keyboard owner update".to_string())
        }
    }

    pub fn set_web_pointer_claimed(claimed: bool) -> Result<(), String> {
        // SAFETY: the scalar is copied synchronously on the main actor.
        if unsafe { qmux_native_terminal_set_web_pointer_claimed(i32::from(claimed)) } == 1 {
            Ok(())
        } else {
            Err("native terminal host is not attached".to_string())
        }
    }

    /// Marks whether DOM focus sits inside a cross-document iframe (the
    /// browser overlay's page). While set, the native key monitor claims
    /// recognized ⌘ app shortcuts even though the responder is a healthy
    /// WKWebView descendant — the host document's window-level handlers never
    /// see keys typed into a framed document.
    pub fn set_iframe_shortcut_fallback(active: bool) -> Result<(), String> {
        // SAFETY: the scalar is copied synchronously on the main actor.
        if unsafe { qmux_native_terminal_set_iframe_shortcut_fallback(i32::from(active)) } == 1 {
            Ok(())
        } else {
            Err("native terminal host is not attached".to_string())
        }
    }

    pub fn set_web_overlay_region(region: NativeWebOverlayRegion) -> Result<(), String> {
        if !region.x.is_finite()
            || !region.y.is_finite()
            || !region.width.is_finite()
            || !region.height.is_finite()
            || region.width < 0.0
            || region.height < 0.0
        {
            return Err("native web overlay region has invalid geometry".to_string());
        }
        let region_id = cstring(&region.region_id, "region id")?;
        // SAFETY: Swift copies the string and scalars synchronously.
        if unsafe {
            qmux_native_terminal_set_web_overlay_region(
                region_id.as_ptr(),
                region.x,
                region.y,
                region.width,
                region.height,
                i32::from(region.visible),
            )
        } == 1
        {
            Ok(())
        } else {
            Err("native terminal host is not attached".to_string())
        }
    }

    pub fn focus(pane_id: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        // SAFETY: Swift copies the string synchronously before returning.
        if unsafe { qmux_native_terminal_focus(pane_id.as_ptr()) } == 1 {
            Ok(())
        } else {
            Err("native terminal pane is unavailable or hidden".to_string())
        }
    }

    pub fn send_text(pane_id: &str, text: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        let text = cstring(text, "terminal input")?;
        // SAFETY: Swift copies both strings synchronously before returning.
        if unsafe { qmux_native_terminal_send_text(pane_id.as_ptr(), text.as_ptr()) } == 1 {
            Ok(())
        } else {
            Err("native terminal pane was not found or its surface is not ready".to_string())
        }
    }

    pub fn submit(pane_id: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        // SAFETY: Swift copies the string and dispatches a synthetic Return key
        // press/release synchronously on the main actor.
        if unsafe { qmux_native_terminal_submit(pane_id.as_ptr()) } == 1 {
            Ok(())
        } else {
            Err("native terminal pane was not found, not ready, or rejected submit".to_string())
        }
    }

    pub fn paste_approved_text(pane_id: &str, text: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        // SAFETY: Swift copies the UTF-8 bytes synchronously before returning.
        if unsafe {
            qmux_native_terminal_paste_approved_text(
                pane_id.as_ptr(),
                text.as_bytes().as_ptr(),
                text.len(),
            )
        } == 1
        {
            Ok(())
        } else {
            Err("native terminal pane was not found or rejected the paste".to_string())
        }
    }

    pub fn action(pane_id: &str, action: &str) -> Result<(), String> {
        let pane_id = cstring(pane_id, "pane id")?;
        let action = cstring(action, "terminal action")?;
        // SAFETY: Swift copies both strings synchronously before returning.
        if unsafe { qmux_native_terminal_action(pane_id.as_ptr(), action.as_ptr()) } == 1 {
            Ok(())
        } else {
            Err("native terminal action was not performed".to_string())
        }
    }

    pub fn update_settings(settings: NativeTerminalSettings) -> Result<(), String> {
        if !settings.font_size.is_finite()
            || settings.font_size <= 0.0
            || !settings.letter_spacing.is_finite()
            || !settings.line_height.is_finite()
            || settings.line_height <= 0.0
            || !settings.scroll_sensitivity.is_finite()
            || settings.scroll_sensitivity <= 0.0
        {
            return Err("native terminal settings contain invalid numeric values".to_string());
        }
        let pane_id = cstring(&settings.pane_id, "pane id")?;
        let font_family = cstring(&settings.font_family, "font family")?;
        let cursor_style = cstring(&settings.cursor_style, "cursor style")?;
        let theme_name = cstring(&settings.theme_name, "theme name")?;
        // SAFETY: Swift copies strings and scalar settings synchronously.
        if unsafe {
            qmux_native_terminal_update_settings(
                pane_id.as_ptr(),
                settings.font_size,
                font_family.as_ptr(),
                settings.letter_spacing,
                settings.line_height,
                i32::from(settings.cursor_blink),
                cursor_style.as_ptr(),
                settings.scrollback_rows,
                i32::from(settings.scroll_on_user_input),
                settings.scroll_sensitivity,
                i32::from(settings.copy_on_select),
                i32::from(settings.selection_clear_on_copy),
                theme_name.as_ptr(),
            )
        } == 1
        {
            Ok(())
        } else {
            Err("native terminal pane was not found or rejected its settings".to_string())
        }
    }

    /// Hands Swift a pane-independent settings snapshot to cache, so panes
    /// created later can build their Ghostty surface at creation time instead
    /// of waiting for their first per-pane settings update.
    pub fn seed_settings(settings: NativeTerminalSeedSettings) -> Result<(), String> {
        if !settings.font_size.is_finite()
            || settings.font_size <= 0.0
            || !settings.letter_spacing.is_finite()
            || !settings.line_height.is_finite()
            || settings.line_height <= 0.0
            || !settings.scroll_sensitivity.is_finite()
            || settings.scroll_sensitivity <= 0.0
        {
            return Err("native terminal settings contain invalid numeric values".to_string());
        }
        let font_family = cstring(&settings.font_family, "font family")?;
        let cursor_style = cstring(&settings.cursor_style, "cursor style")?;
        let theme_name = cstring(&settings.theme_name, "theme name")?;
        // SAFETY: Swift copies strings and scalar settings synchronously.
        if unsafe {
            qmux_native_terminal_seed_settings(
                settings.font_size,
                font_family.as_ptr(),
                settings.letter_spacing,
                settings.line_height,
                i32::from(settings.cursor_blink),
                cursor_style.as_ptr(),
                settings.scrollback_rows,
                i32::from(settings.scroll_on_user_input),
                settings.scroll_sensitivity,
                i32::from(settings.copy_on_select),
                i32::from(settings.selection_clear_on_copy),
                theme_name.as_ptr(),
            )
        } == 1
        {
            Ok(())
        } else {
            Err("native terminal host rejected the settings seed".to_string())
        }
    }

    /// Returns the theme catalog as JSON: the qmux default plus every bundled
    /// Ghostty color scheme, with the colors the settings UI needs for
    /// previews.
    pub fn theme_catalog() -> Result<String, String> {
        // SAFETY: Swift returns a pointer to a process-lifetime JSON buffer
        // (or null if encoding failed); the bytes are copied before returning.
        let catalog = unsafe { qmux_native_terminal_theme_catalog() };
        if catalog.is_null() {
            return Err("native terminal theme catalog is unavailable".to_string());
        }
        // SAFETY: the pointer is non-null, NUL-terminated, and never freed.
        unsafe { std::ffi::CStr::from_ptr(catalog) }
            .to_str()
            .map(ToString::to_string)
            .map_err(|_| "native terminal theme catalog is not valid UTF-8".to_string())
    }

    pub fn shutdown() {
        // SAFETY: shutdown is idempotent and synchronously tears down Swift-owned
        // views on the main thread.
        unsafe { qmux_native_terminal_shutdown() };
        if let Some(state) = APP_STATE.get()
            && let Ok(mut state) = state.lock()
        {
            *state = None;
        }
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
mod imp {
    use super::NativeTerminalKeyboardOwnerUpdate;
    use super::NativeTerminalLayout;
    use super::NativeTerminalSeedSettings;
    use super::NativeTerminalSettings;
    use super::NativeWebOverlayRegion;
    use crate::state::AppState;
    use std::ffi::c_void;

    pub fn available() -> bool {
        false
    }

    pub fn initialize(_native_view: *mut c_void, _state: AppState) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn create_host_managed(
        _pane_id: &str,
        _working_directory: Option<&str>,
    ) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn receive(_pane_id: &str, _bytes: &[u8], _replay: bool) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn is_ready_for_replay(_pane_id: &str) -> Result<bool, String> {
        // The portable renderer replays through the webview, which has no
        // pre-layout default grid to protect; never defer.
        Ok(true)
    }

    pub fn remove(_pane_id: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn set_stage_backstop(_x: f64, _y: f64, _width: f64, _height: f64) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn set_layout(_layout: NativeTerminalLayout) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn set_keyboard_owner(_update: NativeTerminalKeyboardOwnerUpdate) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn set_web_pointer_claimed(_claimed: bool) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn set_web_overlay_region(_region: NativeWebOverlayRegion) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn set_iframe_shortcut_fallback(_active: bool) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn focus(_pane_id: &str) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn send_text(_pane_id: &str, _text: &str) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn submit(_pane_id: &str) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn paste_approved_text(_pane_id: &str, _text: &str) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn action(_pane_id: &str, _action: &str) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn update_settings(_settings: NativeTerminalSettings) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn seed_settings(_settings: NativeTerminalSeedSettings) -> Result<(), String> {
        Err("native terminals are only available on macOS".to_string())
    }

    pub fn theme_catalog() -> Result<String, String> {
        // No native themes off macOS; the settings UI treats an empty catalog
        // as "only the built-in default is available".
        Ok("[]".to_string())
    }

    pub fn shutdown() {}
}

#[allow(unused_imports)]
pub use imp::{
    action, available, create_host_managed, focus, initialize, is_ready_for_replay,
    paste_approved_text, receive, remove, seed_settings, send_text, set_iframe_shortcut_fallback,
    set_layout, set_stage_backstop, set_web_overlay_region, set_web_pointer_claimed, shutdown,
    submit, update_settings,
};

fn with_app_state(operation: impl FnOnce(&AppState)) {
    let Some(slot) = APP_STATE.get() else { return };
    let Ok(state) = slot.lock() else { return };
    let Some(state) = state.as_ref() else { return };
    operation(state);
}

fn callback_string(pointer: *const std::ffi::c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: Swift supplies a valid, NUL-terminated UTF-8 string for the
    // synchronous duration of each callback.
    unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_str()
        .ok()
        .map(ToString::to_string)
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_close(
    pane_id: *const std::ffi::c_char,
    process_alive: i32,
) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    with_app_state(|state| {
        // This delegate fires on the main thread, but the close handler walks
        // the pane's process tree (a `ps` fork), captures the pane (model lock
        // + scrollback read), and drains waiter queues — and a queue drain
        // submits turns, which takes pane send locks the main thread must
        // never contend for (see write_pane). Hand the whole sequence to a
        // worker; the handler's closing-set and pane-registration checks make
        // a deferred or duplicate delivery a no-op.
        let state = state.clone();
        std::thread::spawn(move || {
            crate::pty::native_pane_did_close(&state, &pane_id, process_alive == 1);
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_change_title(
    pane_id: *const std::ffi::c_char,
    title: *const std::ffi::c_char,
) {
    let (Some(pane_id), Some(title)) = (callback_string(pane_id), callback_string(title)) else {
        return;
    };
    // Fires on the AppKit main thread (agents stream OSC titles at startup and
    // while working); the model lock + persist mark happen on the worker.
    let _ = DEFERRED_REPORT_TX.send(DeferredPaneReport::Title { pane_id, title });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_change_cwd(
    pane_id: *const std::ffi::c_char,
    cwd: *const std::ffi::c_char,
) {
    let (Some(pane_id), Some(cwd)) = (callback_string(pane_id), callback_string(cwd)) else {
        return;
    };
    // Fires on the AppKit main thread; the directory stat, model lock, and
    // persist mark happen on the worker.
    let _ = DEFERRED_REPORT_TX.send(DeferredPaneReport::Cwd { pane_id, cwd });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_resize(
    pane_id: *const std::ffi::c_char,
    columns: i32,
    rows: i32,
) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    let (Ok(columns), Ok(rows)) = (u16::try_from(columns), u16::try_from(rows)) else {
        return;
    };
    if columns == 0 || rows == 0 {
        return;
    }
    with_app_state(|state| {
        if let Err(err) = crate::pty::resize_native_host_pane(state, &pane_id, columns, rows) {
            eprintln!("qmux: failed to resize native pane {pane_id}: {err}");
        }
        // A grid resize is also the surest sign the surface can replay at its
        // real width now; finish any attach that was parked waiting for it.
        // Cheap when nothing is parked (a set lookup), and safe on the main
        // thread — the flush itself is handed to a worker.
        crate::pty::complete_pending_attach(state, &pane_id);
    });
}

/// Fired once per pane, from the first `applyGeometry` that fits the surface
/// to a real (nonzero) frame. Completes an attach that `pane_attach` parked
/// because replaying scrollback into the pre-layout default grid would be
/// reflowed — and scrambled — by this very fit.
#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_commit_geometry(pane_id: *const std::ffi::c_char) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    with_app_state(|state| crate::pty::complete_pending_attach(state, &pane_id));
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_write(
    pane_id: *const std::ffi::c_char,
    bytes: *const u8,
    bytes_len: usize,
) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    if bytes.is_null() || bytes_len == 0 {
        return;
    }
    if REPLAYING_PANES
        .lock()
        .is_ok_and(|panes| panes.contains(&pane_id))
    {
        return;
    }
    // SAFETY: Swift keeps the Data buffer alive for the synchronous duration
    // of this callback; copy it before returning across FFI.
    let bytes = unsafe { std::slice::from_raw_parts(bytes, bytes_len) }.to_vec();
    with_app_state(|state| {
        if let Err(err) = crate::pty::write_native_host_input(state, &pane_id, bytes) {
            eprintln!("qmux: failed to write native terminal input for pane {pane_id}: {err}");
        }
    });
}

fn emit_native_event(event_type: &str, pane_id: *const std::ffi::c_char) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            event_type,
            Some(pane_id),
            None,
            serde_json::json!({}),
        ));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_request_search(pane_id: *const std::ffi::c_char) {
    emit_native_event("terminal.search_requested", pane_id);
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_request_paste(
    pane_id: *const std::ffi::c_char,
    text: *const std::ffi::c_char,
) {
    let (Some(pane_id), Some(text)) = (callback_string(pane_id), callback_string(text)) else {
        return;
    };
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            "terminal.paste_requested",
            Some(pane_id),
            None,
            serde_json::json!({ "text": text }),
        ));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_receive_user_input(pane_id: *const std::ffi::c_char) {
    emit_native_event("terminal.user_input", pane_id);
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_receive_escape(pane_id: *const std::ffi::c_char) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    with_app_state(|state| crate::workspace::watch_agent_after_escape(state, &pane_id));
}

/// A possible application shortcut typed while a native pane owned the
/// keyboard. Only exact qmux commands are consumed; every unrecognized chord
/// returns to AppKit/Ghostty unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_receive_shortcut(
    pane_id: *const std::ffi::c_char,
    key: *const std::ffi::c_char,
    shift: i32,
    control: i32,
    option: i32,
    command: i32,
    repeat: i32,
) -> i32 {
    let (Some(pane_id), Some(key)) = (callback_string(pane_id), callback_string(key)) else {
        return 0;
    };
    // Claiming a chord is a promise the frontend will execute it. Without a
    // live event listener the emit below is dropped, so decline instead —
    // the chord stays in the native responder chain rather than being
    // consumed into nothing.
    if !events_listener_ready() {
        return 0;
    }
    let Some(shortcut) =
        classify_app_shortcut(&key, shift == 1, control == 1, option == 1, command == 1)
    else {
        return 0;
    };
    let (command, tab_index) = shortcut.event_fields();
    let mut emitted = false;
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            "terminal.shortcut",
            Some(pane_id),
            None,
            serde_json::json!({
                "command": command,
                "tabIndex": tab_index,
                "repeat": repeat == 1,
            }),
        ));
        emitted = true;
    });
    i32::from(emitted)
}

/// A possible application shortcut typed while web keyboard ownership is
/// stranded outside a healthy WebKit content responder. Emit the same semantic
/// command without tying it to a terminal pane.
#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_receive_app_shortcut(
    key: *const std::ffi::c_char,
    shift: i32,
    control: i32,
    option: i32,
    command: i32,
    repeat: i32,
) -> i32 {
    let Some(key) = callback_string(key) else {
        return 0;
    };
    // Same delivery gate as the terminal-scoped classifier above: never
    // consume a chord the frontend cannot receive.
    if !events_listener_ready() {
        return 0;
    }
    let Some(shortcut) =
        classify_web_app_shortcut(&key, shift == 1, control == 1, option == 1, command == 1)
    else {
        return 0;
    };
    let (command, tab_index) = shortcut.event_fields();
    let mut emitted = false;
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            "app.shortcut",
            None,
            None,
            serde_json::json!({
                "command": command,
                "tabIndex": tab_index,
                "repeat": repeat == 1,
            }),
        ));
        emitted = true;
    });
    i32::from(emitted)
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_change_command_modifier(
    pane_id: *const std::ffi::c_char,
    active: i32,
) {
    let Some(pane_id) = callback_string(pane_id) else {
        return;
    };
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            "terminal.command_modifier_changed",
            Some(pane_id),
            None,
            serde_json::json!({ "active": active == 1 }),
        ));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_activate(pane_id: *const std::ffi::c_char) {
    emit_native_event("terminal.activated", pane_id);
}

#[unsafe(no_mangle)]
pub extern "C" fn qmux_native_terminal_did_open_url(
    pane_id: *const std::ffi::c_char,
    url: *const std::ffi::c_char,
) {
    let (Some(pane_id), Some(url)) = (callback_string(pane_id), callback_string(url)) else {
        return;
    };
    with_app_state(|state| {
        state.emit(QmuxEvent::new(
            "terminal.open_url",
            Some(pane_id),
            None,
            serde_json::json!({ "url": url }),
        ));
    });
}

// The layout/focus/keyboard-owner commands below run during a switch to a
// terminal pane and block on the AppKit main thread; each is timed so a slow
// bridge call lands in the freeze-diagnostics log (see diagnostics.rs).
#[tauri::command]
pub fn native_terminal_set_layout(
    state: tauri::State<'_, AppState>,
    layout: NativeTerminalLayout,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let result = set_layout(layout);
    state
        .diagnostics()
        .note_native_call("set_layout", started.elapsed());
    result
}

#[tauri::command]
pub fn native_terminal_set_keyboard_owner(
    state: tauri::State<'_, AppState>,
    update: NativeTerminalKeyboardOwnerUpdate,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let result = imp::set_keyboard_owner(update);
    state
        .diagnostics()
        .note_native_call("set_keyboard_owner", started.elapsed());
    result
}

#[tauri::command]
pub fn native_terminal_set_web_pointer_claimed(claimed: bool) -> Result<(), String> {
    set_web_pointer_claimed(claimed)
}

#[tauri::command]
pub fn native_terminal_set_web_overlay_region(
    region: NativeWebOverlayRegion,
) -> Result<(), String> {
    set_web_overlay_region(region)
}

#[tauri::command]
pub fn native_terminal_set_iframe_shortcut_fallback(active: bool) -> Result<(), String> {
    set_iframe_shortcut_fallback(active)
}

#[tauri::command]
pub fn native_terminal_set_stage_backstop(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    set_stage_backstop(x, y, width, height)
}

#[tauri::command]
pub fn native_terminal_focus(
    state: tauri::State<'_, AppState>,
    pane_id: String,
) -> Result<(), String> {
    let started = std::time::Instant::now();
    let result = focus(&pane_id);
    state
        .diagnostics()
        .note_native_call("focus", started.elapsed());
    result
}

#[tauri::command]
pub fn native_terminal_action(pane_id: String, action: String) -> Result<(), String> {
    imp::action(&pane_id, &action)
}

#[tauri::command]
pub fn native_terminal_paste_approved_text(pane_id: String, text: String) -> Result<(), String> {
    // The paste boundary must stay unforgeable here just like on the pane_write
    // path: Ghostty frames these bytes in bracketed-paste markers, and the qmux
    // approval dialog this text passed through suppresses Ghostty's own
    // unsafe-paste prompt — so an embedded end marker would terminate the paste
    // early and hand the remainder to the shell as typed input.
    let text = crate::pty::strip_bracketed_paste_markers(&text);
    imp::paste_approved_text(&pane_id, &text)
}

#[tauri::command]
pub fn native_terminal_update_settings(settings: NativeTerminalSettings) -> Result<(), String> {
    imp::update_settings(settings)
}

#[tauri::command]
pub fn native_terminal_seed_settings(settings: NativeTerminalSeedSettings) -> Result<(), String> {
    imp::seed_settings(settings)
}

#[tauri::command]
pub fn native_terminal_theme_catalog() -> Result<String, String> {
    imp::theme_catalog()
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn swift_ghostty_bridge_is_linked() {
        assert!(super::available());
    }

    #[test]
    fn swift_web_app_shortcut_routing_preserves_terminal_and_dom_ownership() {
        const OUTSIDE_WEB_VIEW: i32 = 0;
        const OUTER_WEB_VIEW: i32 = 1;
        const WEB_VIEW_DESCENDANT: i32 = 2;

        for responder_state in [OUTSIDE_WEB_VIEW, OUTER_WEB_VIEW, WEB_VIEW_DESCENDANT] {
            for iframe_fallback_eligible in [false, true] {
                assert!(!super::imp::should_claim_web_app_shortcut(
                    true,
                    responder_state,
                    iframe_fallback_eligible
                ));
            }
        }
        assert!(super::imp::should_claim_web_app_shortcut(
            false,
            OUTSIDE_WEB_VIEW,
            false
        ));
        assert!(super::imp::should_claim_web_app_shortcut(
            false,
            OUTER_WEB_VIEW,
            false
        ));
        assert!(!super::imp::should_claim_web_app_shortcut(
            false,
            WEB_VIEW_DESCENDANT,
            false
        ));
        // A ⌘ chord typed while a cross-document iframe holds DOM focus must
        // be claimed natively: the host document's window-level handlers never
        // see keys delivered to the framed document.
        assert!(super::imp::should_claim_web_app_shortcut(
            false,
            WEB_VIEW_DESCENDANT,
            true
        ));
    }

    #[test]
    fn rejects_invalid_layout_geometry_before_ffi() {
        let result = super::set_layout(super::NativeTerminalLayout {
            pane_id: "pane-1".to_string(),
            x: f64::NAN,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            visible: true,
            accepts_pointer_input: true,
            accepts_keyboard_claim: true,
            defer_geometry: false,
        });
        assert_eq!(
            result.unwrap_err(),
            "native terminal layout contains invalid geometry"
        );
    }

    #[test]
    fn rejects_invalid_backstop_geometry_before_ffi() {
        let result = super::set_stage_backstop(0.0, 0.0, -1.0, 100.0);
        assert_eq!(
            result.unwrap_err(),
            "native terminal backstop has invalid geometry"
        );
    }

    #[test]
    fn native_shortcut_classifier_only_claims_qmux_commands() {
        use super::AppShortcutCommand;

        assert_eq!(
            super::classify_app_shortcut("t", false, false, false, true),
            Some(AppShortcutCommand::NewPane)
        );
        assert_eq!(
            super::classify_app_shortcut("n", false, false, false, true),
            Some(AppShortcutCommand::HomeOrCycleAdapter)
        );
        assert_eq!(
            super::classify_app_shortcut("n", true, false, false, true),
            Some(AppShortcutCommand::NewGroup)
        );
        assert_eq!(
            super::classify_app_shortcut("Tab", true, true, false, false),
            Some(AppShortcutCommand::CyclePaneTab(-1))
        );
        assert_eq!(
            super::classify_app_shortcut("4", false, true, false, false),
            Some(AppShortcutCommand::FocusTab(3))
        );
        assert_eq!(
            super::classify_app_shortcut("r", true, false, false, true),
            Some(AppShortcutCommand::FocusResearchMode)
        );
        assert_eq!(
            super::classify_app_shortcut("`", false, false, false, true),
            Some(AppShortcutCommand::ToggleSidebarMode)
        );
        assert_eq!(
            super::classify_app_shortcut("ArrowUp", false, false, true, true),
            Some(AppShortcutCommand::MoveSidebarItem(-1))
        );
        assert_eq!(
            super::classify_app_shortcut("ArrowDown", false, false, true, true),
            Some(AppShortcutCommand::MoveSidebarItem(1))
        );
        assert_eq!(
            super::classify_app_shortcut("ArrowUp", false, false, true, false),
            None
        );
        assert_eq!(
            AppShortcutCommand::MoveSidebarItem(-1).event_fields(),
            ("moveSidebarItemUp", None)
        );
        assert_eq!(
            super::classify_app_shortcut("j", false, false, false, true),
            Some(AppShortcutCommand::FocusFollowups)
        );
        assert_eq!(
            super::classify_app_shortcut("o", false, false, false, true),
            Some(AppShortcutCommand::OpenFolderMenu)
        );
        assert_eq!(
            super::classify_app_shortcut("j", true, false, false, true),
            None,
            "shift-command-j is not a qmux chord"
        );
        for key in [";", "k", "a", "z", "Enter"] {
            assert_eq!(
                super::classify_app_shortcut(key, false, false, false, true),
                None,
                "command-{key} must remain native"
            );
        }
        assert_eq!(
            super::classify_web_app_shortcut("k", false, false, false, true),
            Some(AppShortcutCommand::OpenCommandPalette)
        );
        assert_eq!(
            super::classify_web_app_shortcut("`", false, false, false, true),
            Some(AppShortcutCommand::ToggleSidebarMode)
        );
        assert_eq!(
            super::classify_web_app_shortcut(",", false, false, false, true),
            Some(AppShortcutCommand::OpenSettings)
        );
        assert_eq!(
            super::classify_app_shortcut("w", false, true, false, false),
            None,
            "control-w belongs to a focused terminal"
        );
    }
}
