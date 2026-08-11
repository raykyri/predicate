use crate::{persistence, state::AppState};
use serde::Serialize;
use std::{path::Path, str::FromStr, sync::Mutex};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutEvent, ShortcutState,
};

pub struct ShowHideShortcutState {
    inner: Mutex<ShowHideShortcutInner>,
    operation: Mutex<()>,
}

impl Default for ShowHideShortcutState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ShowHideShortcutInner::default()),
            operation: Mutex::new(()),
        }
    }
}

#[derive(Default)]
struct ShowHideShortcutInner {
    configured_accelerator: Option<String>,
    registered_accelerator: Option<String>,
    capture_fallback_accelerator: Option<String>,
    last_error: Option<String>,
    capture_active: bool,
}

#[derive(Clone, Debug)]
struct ShowHideShortcutSnapshot {
    configured_accelerator: Option<String>,
    registered_accelerator: Option<String>,
    capture_fallback_accelerator: Option<String>,
    capture_active: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShowHideShortcutSetting {
    pub accelerator: Option<String>,
    pub registered: bool,
    pub error: Option<String>,
    pub capture_active: bool,
}

pub fn init<R: Runtime>(app: &AppHandle<R>, workspace_root: &Path) {
    let accelerator = match persistence::load_preferences(workspace_root) {
        Ok(preferences) => preferences.show_hide_shortcut,
        Err(err) => {
            eprintln!("qmux: failed to read show/hide shortcut preference: {err}");
            None
        }
    };

    let state = app.state::<ShowHideShortcutState>();
    let _operation = state.operation.lock().unwrap();
    match normalize_accelerator(accelerator.clone()) {
        Ok(configured_accelerator) => {
            let registry = TauriShortcutRegistry { app };
            let registration = configured_accelerator
                .as_deref()
                .map(|accelerator| registry.register(accelerator));
            let mut inner = state.inner.lock().unwrap();
            inner.configured_accelerator = configured_accelerator.clone();
            match registration {
                Some(Ok(())) => inner.registered_accelerator = configured_accelerator,
                Some(Err(err)) => {
                    inner.last_error = Some(err.clone());
                    eprintln!("qmux: failed to register show/hide shortcut: {err}");
                }
                None => {}
            }
        }
        Err(err) => {
            let mut inner = state.inner.lock().unwrap();
            inner.configured_accelerator = accelerator;
            inner.last_error = Some(err.clone());
            eprintln!("qmux: invalid show/hide shortcut preference: {err}");
        }
    }
}

pub fn handle_global_shortcut<R: Runtime>(
    app: &AppHandle<R>,
    shortcut: &Shortcut,
    event: ShortcutEvent,
) {
    if event.state != ShortcutState::Pressed {
        return;
    }
    if !app
        .try_state::<ShowHideShortcutState>()
        .is_some_and(|state| state.handles(shortcut))
    {
        return;
    }
    if let Err(err) = toggle_qmux_visibility(app) {
        eprintln!("qmux: failed to toggle app visibility: {err}");
    }
}

#[tauri::command]
pub fn show_hide_shortcut_get(
    shortcut_state: tauri::State<'_, ShowHideShortcutState>,
) -> Result<ShowHideShortcutSetting, String> {
    Ok(shortcut_state.status())
}

// Async (like the blocking commands in main.rs): both setters rewrite the
// fsync'd preferences file and talk to the OS shortcut registry, which would
// otherwise stall the main thread. The getter reads in-memory state and stays
// synchronous.
#[tauri::command(async)]
pub fn show_hide_shortcut_set<R: Runtime>(
    app: AppHandle<R>,
    app_state: tauri::State<'_, AppState>,
    shortcut_state: tauri::State<'_, ShowHideShortcutState>,
    accelerator: Option<String>,
) -> Result<ShowHideShortcutSetting, String> {
    let accelerator = normalize_accelerator(accelerator)?;
    let _operation = shortcut_state.operation.lock().unwrap();
    let snapshot = shortcut_state.snapshot();
    let registry = TauriShortcutRegistry { app: &app };
    let persist = || {
        persistence::update_preferences(&app_state.config().workspace_root, |preferences| {
            preferences.show_hide_shortcut = accelerator.clone();
        })
    };

    let result = if snapshot.capture_active {
        probe_registration(&registry, accelerator.as_deref()).and_then(|()| persist())
    } else {
        replace_active_registration(
            &registry,
            snapshot.registered_accelerator.as_deref(),
            accelerator.as_deref(),
            persist,
        )
    };

    let mut inner = shortcut_state.inner.lock().unwrap();
    match result {
        Ok(()) => {
            inner.configured_accelerator = accelerator.clone();
            inner.registered_accelerator = if snapshot.capture_active {
                None
            } else {
                accelerator
            };
            inner.last_error = None;
        }
        Err(err) => {
            inner.configured_accelerator = snapshot.configured_accelerator;
            inner.registered_accelerator = registered_candidate(
                &registry,
                [
                    snapshot.registered_accelerator.as_deref(),
                    accelerator.as_deref(),
                ],
            );
            inner.last_error = Some(err);
        }
    }
    drop(inner);
    Ok(shortcut_state.status())
}

#[tauri::command(async)]
pub fn show_hide_shortcut_capture_set<R: Runtime>(
    app: AppHandle<R>,
    app_state: tauri::State<'_, AppState>,
    shortcut_state: tauri::State<'_, ShowHideShortcutState>,
    active: bool,
) -> Result<ShowHideShortcutSetting, String> {
    let _operation = shortcut_state.operation.lock().unwrap();
    let snapshot = shortcut_state.snapshot();
    if snapshot.capture_active == active {
        return Ok(shortcut_state.status());
    }
    let registry = TauriShortcutRegistry { app: &app };

    if active {
        if let Some(registered) = snapshot.registered_accelerator.as_deref()
            && let Err(err) = registry.unregister(registered)
        {
            shortcut_state.set_error(err);
            return Ok(shortcut_state.status());
        }
        let mut inner = shortcut_state.inner.lock().unwrap();
        inner.capture_active = true;
        inner.capture_fallback_accelerator = snapshot.configured_accelerator;
        inner.registered_accelerator = None;
        inner.last_error = None;
        drop(inner);
        return Ok(shortcut_state.status());
    }

    finish_shortcut_capture(
        &registry,
        &app_state.config().workspace_root,
        shortcut_state.inner(),
        snapshot,
    );
    Ok(shortcut_state.status())
}

impl ShowHideShortcutState {
    fn set_error(&self, error: String) {
        self.inner.lock().unwrap().last_error = Some(error);
    }

    fn snapshot(&self) -> ShowHideShortcutSnapshot {
        let inner = self.inner.lock().unwrap();
        ShowHideShortcutSnapshot {
            configured_accelerator: inner.configured_accelerator.clone(),
            registered_accelerator: inner.registered_accelerator.clone(),
            capture_fallback_accelerator: inner.capture_fallback_accelerator.clone(),
            capture_active: inner.capture_active,
        }
    }

    fn handles(&self, shortcut: &Shortcut) -> bool {
        let inner = self.inner.lock().unwrap();
        if inner.capture_active {
            return false;
        }
        inner
            .registered_accelerator
            .as_deref()
            .and_then(|accelerator| Shortcut::from_str(accelerator).ok())
            .is_some_and(|registered| registered == *shortcut)
    }

    fn status(&self) -> ShowHideShortcutSetting {
        let inner = self.inner.lock().unwrap();
        let registered = match (
            inner.configured_accelerator.as_deref(),
            inner.registered_accelerator.as_deref(),
        ) {
            (Some(configured), Some(registered)) => configured == registered,
            _ => false,
        };
        ShowHideShortcutSetting {
            accelerator: inner.configured_accelerator.clone(),
            registered,
            error: inner.last_error.clone(),
            capture_active: inner.capture_active,
        }
    }
}

fn normalize_accelerator(accelerator: Option<String>) -> Result<Option<String>, String> {
    let Some(accelerator) = accelerator else {
        return Ok(None);
    };
    let accelerator = accelerator.trim();
    if accelerator.is_empty() {
        return Ok(None);
    }

    let parts = accelerator.split('+').map(str::trim).collect::<Vec<&str>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err("Shortcut contains an empty key segment.".to_string());
    }

    let shortcut_text = parts.join("+");
    let shortcut = Shortcut::from_str(&shortcut_text)
        .map_err(|err| format!("Couldn't use that shortcut: {err}"))?;
    if shortcut.mods.is_empty() {
        return Err("Use at least one modifier, such as Option or Command.".to_string());
    }

    Ok(Some(display_accelerator(shortcut)))
}

pub(crate) trait ShortcutRegistry {
    fn register(&self, accelerator: &str) -> Result<(), String>;
    fn unregister(&self, accelerator: &str) -> Result<(), String>;
    fn is_registered(&self, accelerator: &str) -> bool;
}

struct TauriShortcutRegistry<'a, R: Runtime> {
    app: &'a AppHandle<R>,
}

impl<R: Runtime> ShortcutRegistry for TauriShortcutRegistry<'_, R> {
    fn register(&self, accelerator: &str) -> Result<(), String> {
        self.app
            .global_shortcut()
            .register(accelerator)
            .map_err(|err| format!("{err}"))
    }

    fn unregister(&self, accelerator: &str) -> Result<(), String> {
        self.app
            .global_shortcut()
            .unregister(accelerator)
            .map_err(|err| format!("{err}"))
    }

    fn is_registered(&self, accelerator: &str) -> bool {
        self.app.global_shortcut().is_registered(accelerator)
    }
}

/// Registers the replacement before releasing the current shortcut. Persistence
/// happens only after the OS transition succeeds; a persistence failure restores
/// the previous registration before returning.
pub(crate) fn replace_active_registration(
    registry: &impl ShortcutRegistry,
    previous: Option<&str>,
    next: Option<&str>,
    persist: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if previous == next {
        return persist();
    }

    if let Some(next) = next {
        registry
            .register(next)
            .map_err(|err| format!("Couldn't register {next}: {err}"))?;
    }

    if let Some(previous) = previous
        && let Err(err) = registry.unregister(previous)
    {
        let rollback = next
            .and_then(|next| registry.unregister(next).err())
            .map(|rollback| format!(" The replacement also couldn't be released: {rollback}."))
            .unwrap_or_default();
        return Err(format!(
            "Couldn't release the previous shortcut {previous}: {err}.{rollback}"
        ));
    }

    if let Err(err) = persist() {
        let mut rollback_errors = Vec::new();
        if let Some(previous) = previous
            && let Err(rollback) = registry.register(previous)
        {
            rollback_errors.push(format!("restore {previous}: {rollback}"));
        }
        if let Some(next) = next
            && let Err(rollback) = registry.unregister(next)
        {
            rollback_errors.push(format!("release {next}: {rollback}"));
        }
        if rollback_errors.is_empty() {
            return Err(err);
        }
        return Err(format!(
            "{err}. Registration rollback also failed ({}).",
            rollback_errors.join(", ")
        ));
    }

    Ok(())
}

/// While the capture input is focused no qmux shortcut may stay registered or
/// macOS will consume that chord before the webview can record it. Probe a new
/// chord synchronously, then release it until capture ends.
fn probe_registration(
    registry: &impl ShortcutRegistry,
    accelerator: Option<&str>,
) -> Result<(), String> {
    let Some(accelerator) = accelerator else {
        return Ok(());
    };
    registry
        .register(accelerator)
        .map_err(|err| format!("Couldn't register {accelerator}: {err}"))?;
    registry.unregister(accelerator).map_err(|err| {
        format!("Registered {accelerator}, but couldn't suspend it during capture: {err}")
    })
}

fn registered_candidate<'a>(
    registry: &impl ShortcutRegistry,
    candidates: impl IntoIterator<Item = Option<&'a str>>,
) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .find(|accelerator| registry.is_registered(accelerator))
        .map(str::to_string)
}

fn finish_shortcut_capture(
    registry: &impl ShortcutRegistry,
    workspace_root: &Path,
    state: &ShowHideShortcutState,
    snapshot: ShowHideShortcutSnapshot,
) {
    let Some(configured) = snapshot.configured_accelerator.as_deref() else {
        let mut inner = state.inner.lock().unwrap();
        inner.capture_active = false;
        inner.capture_fallback_accelerator = None;
        inner.registered_accelerator = None;
        inner.last_error = None;
        return;
    };

    let registration = if registry.is_registered(configured) {
        Ok(())
    } else {
        registry.register(configured)
    };
    if registration.is_ok() {
        let mut inner = state.inner.lock().unwrap();
        inner.capture_active = false;
        inner.capture_fallback_accelerator = None;
        inner.registered_accelerator = Some(configured.to_string());
        inner.last_error = None;
        return;
    }

    let registration_error = registration.unwrap_err();
    let fallback = snapshot.capture_fallback_accelerator;
    if fallback.as_deref() == Some(configured) {
        let mut inner = state.inner.lock().unwrap();
        inner.capture_active = false;
        inner.capture_fallback_accelerator = None;
        inner.registered_accelerator =
            registered_candidate(registry, [Some(configured), fallback.as_deref()]);
        inner.last_error = Some(format!(
            "Couldn't reactivate {configured} after capture: {registration_error}"
        ));
        return;
    }

    let restore = replace_active_registration(registry, None, fallback.as_deref(), || {
        persistence::update_preferences(workspace_root, |preferences| {
            preferences.show_hide_shortcut = fallback.clone();
        })
    });
    let mut inner = state.inner.lock().unwrap();
    inner.capture_active = false;
    inner.capture_fallback_accelerator = None;
    match restore {
        Ok(()) => {
            inner.configured_accelerator = fallback.clone();
            inner.registered_accelerator = fallback;
            inner.last_error = Some(format!(
                "Couldn't activate {configured}: {registration_error}. Restored the previous setting."
            ));
        }
        Err(restore_error) => {
            inner.registered_accelerator =
                registered_candidate(registry, [Some(configured), fallback.as_deref()]);
            inner.last_error = Some(format!(
                "Couldn't activate {configured}: {registration_error}. Restoring the previous shortcut also failed: {restore_error}"
            ));
        }
    }
}

pub fn toggle_qmux_visibility<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    // A human-browser page can temporarily give key-window status to WebKit's
    // own AppKit UI (for example a popup or panel) while qmux remains the active
    // application. Tauri's window focus check then reports false and turns a
    // requested hide into a no-op show. Application activation is the stable
    // macOS boundary: it still distinguishes qmux from whichever external app
    // should be replaced when the global shortcut is pressed there.
    #[cfg(target_os = "macos")]
    let app_is_active = crate::native_terminal::application_is_active();
    #[cfg(not(target_os = "macos"))]
    let app_is_active = window.is_focused().unwrap_or(false);

    let should_hide = should_hide_qmux_window(
        window.is_visible().unwrap_or(false),
        window.is_minimized().unwrap_or(false),
        app_is_active,
    );

    if should_hide {
        hide_qmux_window(app)?;
    } else {
        show_qmux_window(app)?;
    }

    Ok(())
}

fn should_hide_qmux_window(is_visible: bool, is_minimized: bool, app_is_active: bool) -> bool {
    is_visible && !is_minimized && app_is_active
}

pub fn show_qmux_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };

    // App-level show/hide is a macOS-only Tauri API (it restores the whole app,
    // including other windows and the Dock state). Other platforms fall back to
    // the window-level calls alone, which also keeps the crate compiling for
    // Linux dev/test builds.
    #[cfg(target_os = "macos")]
    app.show()?;
    window.show()?;
    window.unminimize()?;
    window.set_focus()?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn hide_qmux_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    app.hide()
}

#[cfg(not(target_os = "macos"))]
pub fn hide_qmux_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    window.hide()
}

fn display_accelerator(shortcut: Shortcut) -> String {
    let mut parts = Vec::new();
    if shortcut.mods.contains(Modifiers::CONTROL) {
        parts.push("Control".to_string());
    }
    if shortcut.mods.contains(Modifiers::ALT) {
        parts.push("Option".to_string());
    }
    if shortcut.mods.contains(Modifiers::SHIFT) {
        parts.push("Shift".to_string());
    }
    if shortcut.mods.contains(Modifiers::SUPER) {
        parts.push("Command".to_string());
    }
    parts.push(display_key(shortcut.key));
    parts.join("+")
}

fn display_key(key: Code) -> String {
    use Code::*;
    match key {
        KeyA => "A",
        KeyB => "B",
        KeyC => "C",
        KeyD => "D",
        KeyE => "E",
        KeyF => "F",
        KeyG => "G",
        KeyH => "H",
        KeyI => "I",
        KeyJ => "J",
        KeyK => "K",
        KeyL => "L",
        KeyM => "M",
        KeyN => "N",
        KeyO => "O",
        KeyP => "P",
        KeyQ => "Q",
        KeyR => "R",
        KeyS => "S",
        KeyT => "T",
        KeyU => "U",
        KeyV => "V",
        KeyW => "W",
        KeyX => "X",
        KeyY => "Y",
        KeyZ => "Z",
        Digit0 => "0",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        Space => "Space",
        Enter => "Enter",
        Tab => "Tab",
        Escape => "Escape",
        Backspace => "Backspace",
        Delete => "Delete",
        ArrowUp => "Up",
        ArrowDown => "Down",
        ArrowLeft => "Left",
        ArrowRight => "Right",
        Minus => "-",
        Equal => "=",
        BracketLeft => "[",
        BracketRight => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Comma => ",",
        Period => ".",
        Slash => "/",
        Backquote => "`",
        Home => "Home",
        End => "End",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Insert => "Insert",
        CapsLock => "CapsLock",
        PrintScreen => "PrintScreen",
        ScrollLock => "ScrollLock",
        Pause => "Pause",
        NumLock => "NumLock",
        Numpad0 => "Numpad0",
        Numpad1 => "Numpad1",
        Numpad2 => "Numpad2",
        Numpad3 => "Numpad3",
        Numpad4 => "Numpad4",
        Numpad5 => "Numpad5",
        Numpad6 => "Numpad6",
        Numpad7 => "Numpad7",
        Numpad8 => "Numpad8",
        Numpad9 => "Numpad9",
        NumpadAdd => "NumpadAdd",
        NumpadDecimal => "NumpadDecimal",
        NumpadDivide => "NumpadDivide",
        NumpadEnter => "NumpadEnter",
        NumpadEqual => "NumpadEqual",
        NumpadMultiply => "NumpadMultiply",
        NumpadSubtract => "NumpadSubtract",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        F13 => "F13",
        F14 => "F14",
        F15 => "F15",
        F16 => "F16",
        F17 => "F17",
        F18 => "F18",
        F19 => "F19",
        F20 => "F20",
        F21 => "F21",
        F22 => "F22",
        F23 => "F23",
        F24 => "F24",
        AudioVolumeDown => "VolumeDown",
        AudioVolumeUp => "VolumeUp",
        AudioVolumeMute => "VolumeMute",
        MediaPlay => "MediaPlay",
        MediaPause => "MediaPause",
        MediaPlayPause => "MediaPlayPause",
        MediaStop => "MediaStop",
        MediaTrackNext => "MediaTrackNext",
        MediaTrackPrevious => "MediaTrackPrevious",
        _ => return key.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        collections::HashSet,
        fs,
        rc::Rc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[derive(Default)]
    struct MockRegistry {
        registered: RefCell<HashSet<String>>,
        fail_register: RefCell<HashSet<String>>,
        fail_unregister: RefCell<HashSet<String>>,
        operations: Rc<RefCell<Vec<String>>>,
    }

    impl ShortcutRegistry for MockRegistry {
        fn register(&self, accelerator: &str) -> Result<(), String> {
            self.operations
                .borrow_mut()
                .push(format!("register:{accelerator}"));
            if self.fail_register.borrow().contains(accelerator) {
                return Err("registration refused".to_string());
            }
            if !self.registered.borrow_mut().insert(accelerator.to_string()) {
                return Err("already registered".to_string());
            }
            Ok(())
        }

        fn unregister(&self, accelerator: &str) -> Result<(), String> {
            self.operations
                .borrow_mut()
                .push(format!("unregister:{accelerator}"));
            if self.fail_unregister.borrow().contains(accelerator) {
                return Err("unregistration refused".to_string());
            }
            if !self.registered.borrow_mut().remove(accelerator) {
                return Err("not registered".to_string());
            }
            Ok(())
        }

        fn is_registered(&self, accelerator: &str) -> bool {
            self.registered.borrow().contains(accelerator)
        }
    }

    fn registry_with(accelerator: &str) -> MockRegistry {
        let registry = MockRegistry::default();
        registry
            .registered
            .borrow_mut()
            .insert(accelerator.to_string());
        registry
    }

    fn temp_root() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("qmux-shortcut-{nanos}"))
    }

    #[test]
    fn blank_shortcut_is_none() {
        assert_eq!(normalize_accelerator(None).unwrap(), None);
        assert_eq!(normalize_accelerator(Some("  ".to_string())).unwrap(), None);
    }

    #[test]
    fn shortcut_display_uses_macos_names() {
        assert_eq!(
            normalize_accelerator(Some("Alt + Space".to_string())).unwrap(),
            Some("Option+Space".to_string())
        );
        assert_eq!(
            normalize_accelerator(Some("Command+Shift+A".to_string())).unwrap(),
            Some("Shift+Command+A".to_string())
        );
    }

    #[test]
    fn shortcut_requires_modifier() {
        assert!(normalize_accelerator(Some("Space".to_string())).is_err());
    }

    #[test]
    fn handler_claims_only_the_registered_shortcut_outside_capture() {
        let state = ShowHideShortcutState::default();
        {
            let mut inner = state.inner.lock().unwrap();
            inner.configured_accelerator = Some("Option+Space".to_string());
            inner.registered_accelerator = Some("Option+Space".to_string());
        }
        let registered = Shortcut::from_str("Option+Space").unwrap();
        let other = Shortcut::from_str("Shift+Command+A").unwrap();

        assert!(state.handles(&registered));
        assert!(!state.handles(&other));

        state.inner.lock().unwrap().capture_active = true;
        assert!(!state.handles(&registered));
    }

    #[test]
    fn active_app_hides_even_when_an_auxiliary_browser_window_is_key() {
        assert!(should_hide_qmux_window(true, false, true));
    }

    #[test]
    fn inactive_visible_app_is_shown_instead_of_hidden() {
        assert!(!should_hide_qmux_window(true, false, false));
    }

    #[test]
    fn hidden_or_minimized_active_app_is_shown() {
        assert!(!should_hide_qmux_window(false, false, true));
        assert!(!should_hide_qmux_window(true, true, true));
    }

    #[test]
    fn replacement_registers_new_before_releasing_old_and_persisting() {
        let registry = registry_with("Option+Space");
        let operations = Rc::clone(&registry.operations);

        replace_active_registration(
            &registry,
            Some("Option+Space"),
            Some("Shift+Command+A"),
            || {
                operations.borrow_mut().push("persist".to_string());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            registry.operations.borrow().as_slice(),
            [
                "register:Shift+Command+A",
                "unregister:Option+Space",
                "persist",
            ]
        );
        assert!(registry.is_registered("Shift+Command+A"));
        assert!(!registry.is_registered("Option+Space"));
    }

    #[test]
    fn rejected_replacement_keeps_previous_registration_and_skips_persistence() {
        let registry = registry_with("Option+Space");
        registry
            .fail_register
            .borrow_mut()
            .insert("Shift+Command+A".to_string());
        let persisted = Rc::new(RefCell::new(false));
        let persisted_for_call = Rc::clone(&persisted);

        let result = replace_active_registration(
            &registry,
            Some("Option+Space"),
            Some("Shift+Command+A"),
            || {
                *persisted_for_call.borrow_mut() = true;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!*persisted.borrow());
        assert!(registry.is_registered("Option+Space"));
        assert!(!registry.is_registered("Shift+Command+A"));
    }

    #[test]
    fn persistence_failure_restores_previous_registration() {
        let registry = registry_with("Option+Space");
        let operations = Rc::clone(&registry.operations);

        let result = replace_active_registration(
            &registry,
            Some("Option+Space"),
            Some("Shift+Command+A"),
            || {
                operations.borrow_mut().push("persist".to_string());
                Err("disk full".to_string())
            },
        );

        assert!(result.is_err());
        assert_eq!(
            registry.operations.borrow().as_slice(),
            [
                "register:Shift+Command+A",
                "unregister:Option+Space",
                "persist",
                "register:Option+Space",
                "unregister:Shift+Command+A",
            ]
        );
        assert!(registry.is_registered("Option+Space"));
        assert!(!registry.is_registered("Shift+Command+A"));
    }

    #[test]
    fn capture_probe_releases_the_candidate_immediately() {
        let registry = MockRegistry::default();

        probe_registration(&registry, Some("Option+Space")).unwrap();

        assert_eq!(
            registry.operations.borrow().as_slice(),
            ["register:Option+Space", "unregister:Option+Space"]
        );
        assert!(!registry.is_registered("Option+Space"));
    }

    #[test]
    fn failed_capture_replacement_restores_the_previous_setting() {
        let root = temp_root();
        let mut preferences = persistence::AppPreferences {
            show_hide_shortcut: Some("Shift+Command+A".to_string()),
            ..Default::default()
        };
        persistence::save_preferences(&root, &preferences).unwrap();

        let state = ShowHideShortcutState::default();
        {
            let mut inner = state.inner.lock().unwrap();
            inner.configured_accelerator = Some("Shift+Command+A".to_string());
            inner.capture_fallback_accelerator = Some("Option+Space".to_string());
            inner.capture_active = true;
        }
        let registry = MockRegistry::default();
        registry
            .fail_register
            .borrow_mut()
            .insert("Shift+Command+A".to_string());

        finish_shortcut_capture(&registry, &root, &state, state.snapshot());

        let status = state.status();
        assert_eq!(status.accelerator.as_deref(), Some("Option+Space"));
        assert!(status.registered);
        assert!(!status.capture_active);
        assert!(status.error.is_some());
        preferences = persistence::load_preferences(&root).unwrap();
        assert_eq!(
            preferences.show_hide_shortcut.as_deref(),
            Some("Option+Space")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
