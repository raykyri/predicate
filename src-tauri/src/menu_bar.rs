use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

const TRAY_ID: &str = "qmux-menu-bar";
const SHOW_WINDOW_ID: &str = "qmux-menu-bar-show-window";
const HIDE_WINDOW_ID: &str = "qmux-menu-bar-hide-window";
const SELECT_PANE_PREFIX: &str = "qmux-menu-bar-select-pane:";
const SELECT_PANE_EVENT: &str = "menu-bar-select-pane";
const MAX_TAB_TITLE_CHARS: usize = 40;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MenuBarSnapshot {
    pub groups: Vec<MenuBarGroup>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MenuBarGroup {
    pub id: String,
    pub label: String,
    pub tabs: Vec<MenuBarTab>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MenuBarTab {
    pub pane_id: String,
    pub title: String,
    pub path: Option<String>,
    #[serde(default = "default_status_tone")]
    pub status_tone: String,
    pub status_label: Option<String>,
    #[serde(default)]
    pub waiting_on_pane: bool,
    #[serde(default)]
    pub selected: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectPanePayload {
    pane_id: String,
}

fn default_status_tone() -> String {
    "idle".to_string()
}

/// The last applied tray menu: the snapshot it renders plus one item handle
/// per tab, flattened in render order. Content-only changes (a status dot, a
/// retitled tab) mutate those items in place; a full AppKit menu rebuild —
/// which reconstructs every NSMenuItem on the main thread and swaps the tray
/// menu — is reserved for structural changes (groups or tabs added, removed,
/// or reordered). Agent status flips are by far the most frequent update, and
/// each one previously paid the full rebuild.
#[cfg(target_os = "macos")]
struct AppliedMenuBar {
    snapshot: MenuBarSnapshot,
    tab_items: Vec<tauri::menu::IconMenuItem<tauri::Wry>>,
}

#[cfg(target_os = "macos")]
static APPLIED_MENU_BAR: std::sync::Mutex<Option<AppliedMenuBar>> = std::sync::Mutex::new(None);

#[cfg(target_os = "macos")]
static LATEST_MENU_BAR_SNAPSHOT: std::sync::Mutex<Option<MenuBarSnapshot>> =
    std::sync::Mutex::new(None);

#[cfg(target_os = "macos")]
fn create_tray(
    app: &AppHandle,
    menu: &tauri::menu::Menu<tauri::Wry>,
) -> tauri::Result<tauri::tray::TrayIcon<tauri::Wry>> {
    use tauri::tray::TrayIconBuilder;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(bento_icon())
        .icon_as_template(true)
        .tooltip("qmux")
        .menu(menu)
        .show_menu_on_left_click(true)
        .on_menu_event(handle_menu_event)
        .build(app)
}

#[tauri::command]
pub fn menu_bar_set_visible(app: AppHandle, visible: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            return tray.set_visible(visible).map_err(|err| err.to_string());
        }
        if !visible {
            return Ok(());
        }

        // The frontend sends its current tab snapshot before applying this
        // preference. Holding the snapshot lock through tray creation keeps a
        // simultaneous update from being lost between the initial menu build
        // and the applied-state handoff.
        let latest = LATEST_MENU_BAR_SNAPSHOT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (menu, tab_items) = build_menu(&app, latest.as_ref()).map_err(|err| err.to_string())?;
        create_tray(&app, &menu).map_err(|err| err.to_string())?;

        let mut applied = APPLIED_MENU_BAR
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *applied = latest.as_ref().map(|snapshot| AppliedMenuBar {
            snapshot: snapshot.clone(),
            tab_items,
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        let _ = visible;
    }

    Ok(())
}

#[tauri::command]
pub fn menu_bar_update(app: AppHandle, snapshot: MenuBarSnapshot) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut latest = LATEST_MENU_BAR_SNAPSHOT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *latest = Some(snapshot.clone());
        update_menu(&app, snapshot).map_err(|err| err.to_string())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        let _ = snapshot;
        Ok(())
    }
}

/// Whether `next` can be applied to the already-built menu by mutating items:
/// same groups (id and label) holding the same tabs in the same order. Group
/// labels are structural on purpose — submenu text has no retained handle here,
/// and label changes (a rename, a moved folder) are rare enough that a rebuild
/// is the simpler correct path.
#[cfg(target_os = "macos")]
fn same_menu_structure(current: &MenuBarSnapshot, next: &MenuBarSnapshot) -> bool {
    current.groups.len() == next.groups.len()
        && current.groups.iter().zip(&next.groups).all(|(a, b)| {
            a.id == b.id
                && a.label == b.label
                && a.tabs.len() == b.tabs.len()
                && a.tabs
                    .iter()
                    .zip(&b.tabs)
                    .all(|(tab_a, tab_b)| tab_a.pane_id == tab_b.pane_id)
        })
}

/// Applies a structure-preserving snapshot by mutating only the tabs whose
/// rendered label or status dot actually changed. Tauri marshals each item
/// mutation to the main thread, so the cost is a couple of NSMenuItem edits
/// instead of rebuilding the whole menu.
#[cfg(target_os = "macos")]
fn apply_tab_updates(applied: &AppliedMenuBar, next: &MenuBarSnapshot) -> tauri::Result<()> {
    let current_tabs = applied.snapshot.groups.iter().flat_map(|group| &group.tabs);
    let next_tabs = next.groups.iter().flat_map(|group| &group.tabs);
    for ((current, next), item) in current_tabs.zip(next_tabs).zip(&applied.tab_items) {
        if current == next {
            continue;
        }
        let label = tab_menu_label(next);
        if tab_menu_label(current) != label {
            item.set_text(label)?;
        }
        if (current.status_tone.as_str(), current.waiting_on_pane)
            != (next.status_tone.as_str(), next.waiting_on_pane)
        {
            item.set_icon(Some(status_icon(&next.status_tone, next.waiting_on_pane)))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn update_menu(app: &AppHandle, snapshot: MenuBarSnapshot) -> tauri::Result<()> {
    let mut applied = APPLIED_MENU_BAR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Snapshot updates continue while the icon is disabled so creating it
    // later can render the latest menu immediately.
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        *applied = None;
        return Ok(());
    };
    if let Some(current) = applied.as_mut()
        && same_menu_structure(&current.snapshot, &snapshot)
        && current.tab_items.len()
            == snapshot
                .groups
                .iter()
                .map(|group| group.tabs.len())
                .sum::<usize>()
    {
        apply_tab_updates(current, &snapshot)?;
        current.snapshot = snapshot;
        return Ok(());
    }
    let (menu, tab_items) = build_menu(app, Some(&snapshot))?;
    tray.set_menu(Some(menu))?;
    *applied = Some(AppliedMenuBar {
        snapshot,
        tab_items,
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_menu(
    app: &AppHandle,
    snapshot: Option<&MenuBarSnapshot>,
) -> tauri::Result<(
    tauri::menu::Menu<tauri::Wry>,
    Vec<tauri::menu::IconMenuItem<tauri::Wry>>,
)> {
    use tauri::menu::{IconMenuItemBuilder, Menu, MenuItemBuilder, PredefinedMenuItem, Submenu};

    let menu = Menu::new(app)?;
    let show = MenuItemBuilder::with_id(SHOW_WINDOW_ID, "Show Window").build(app)?;
    let hide = MenuItemBuilder::with_id(HIDE_WINDOW_ID, "Hide Window").build(app)?;
    let separator = PredefinedMenuItem::separator(app)?;
    menu.append(&show)?;
    menu.append(&hide)?;
    menu.append(&separator)?;

    let Some(snapshot) = snapshot else {
        let empty = MenuItemBuilder::new("No tabs").enabled(false).build(app)?;
        menu.append(&empty)?;
        return Ok((menu, Vec::new()));
    };

    let mut tab_items = Vec::new();
    for group in &snapshot.groups {
        let group_label = sanitize_menu_text(&group.label, 88)
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "Group".to_string());
        let group_menu = Submenu::with_id(
            app,
            format!("qmux-menu-bar-group:{}", group.id),
            group_label,
            true,
        )?;

        if group.tabs.is_empty() {
            let empty = MenuItemBuilder::new("No tabs").enabled(false).build(app)?;
            group_menu.append(&empty)?;
        } else {
            for tab in &group.tabs {
                let item = IconMenuItemBuilder::with_id(
                    format!("{SELECT_PANE_PREFIX}{}", tab.pane_id),
                    tab_menu_label(tab),
                )
                .icon(status_icon(&tab.status_tone, tab.waiting_on_pane))
                .build(app)?;
                group_menu.append(&item)?;
                tab_items.push(item);
            }
        }

        menu.append(&group_menu)?;
    }

    if tab_items.is_empty() {
        let empty = MenuItemBuilder::new("No active tabs")
            .enabled(false)
            .build(app)?;
        menu.append(&empty)?;
    }

    Ok((menu, tab_items))
}

#[cfg(target_os = "macos")]
fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        SHOW_WINDOW_ID => {
            if let Err(err) = crate::show_hide_shortcut::show_qmux_window(app) {
                eprintln!("qmux: failed to show app from menu bar: {err}");
            }
        }
        HIDE_WINDOW_ID => {
            if let Err(err) = crate::show_hide_shortcut::hide_qmux_window(app) {
                eprintln!("qmux: failed to hide app from menu bar: {err}");
            }
        }
        id => {
            if let Some(pane_id) = id.strip_prefix(SELECT_PANE_PREFIX) {
                if let Err(err) = crate::show_hide_shortcut::show_qmux_window(app) {
                    eprintln!("qmux: failed to show app from menu bar tab selection: {err}");
                }
                if let Err(err) = app.emit(
                    SELECT_PANE_EVENT,
                    SelectPanePayload {
                        pane_id: pane_id.to_string(),
                    },
                ) {
                    eprintln!("qmux: failed to emit menu bar tab selection: {err}");
                }
            }
        }
    }
}

#[cfg(any(test, target_os = "macos"))]
fn tab_menu_label(tab: &MenuBarTab) -> String {
    let mut label = String::new();
    if tab.selected {
        label.push_str("* ");
    }
    label.push_str(
        &sanitize_menu_text(&tab.title, MAX_TAB_TITLE_CHARS)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled".to_string()),
    );

    if let Some(path) = sanitize_menu_text(tab.path.as_deref().unwrap_or_default(), 96)
        .filter(|path| !path.is_empty())
    {
        label.push_str(" - ");
        label.push_str(&path);
    }

    if let Some(status) = sanitize_menu_text(tab.status_label.as_deref().unwrap_or_default(), 48)
        .filter(|status| !status.is_empty())
    {
        label.push_str(" (");
        label.push_str(&status);
        label.push(')');
    }

    label
}

#[cfg(any(test, target_os = "macos"))]
fn sanitize_menu_text(text: &str, max_chars: usize) -> Option<String> {
    let compact = text
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        return None;
    }
    Some(truncate_chars(&compact, max_chars))
}

#[cfg(any(test, target_os = "macos"))]
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(target_os = "macos")]
fn status_icon(tone: &str, waiting_on_pane: bool) -> tauri::image::Image<'static> {
    let (red, green, blue) = if waiting_on_pane {
        (0xd7, 0xa8, 0x4f)
    } else {
        match tone {
            "active" => (0xd7, 0xa8, 0x4f),
            "pending" => (0x7f, 0x88, 0x84),
            "attention" => (0xe0, 0x79, 0x6d),
            "done" => (0x6c, 0xae, 0x9d),
            "error" => (0xe0, 0x8a, 0x5f),
            _ => (0x7f, 0x88, 0x84),
        }
    };
    let outline = waiting_on_pane || tone == "idle";
    dot_icon(red, green, blue, outline)
}

#[cfg(target_os = "macos")]
fn dot_icon(red: u8, green: u8, blue: u8, outline: bool) -> tauri::image::Image<'static> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    const SIZE: u32 = 18;

    // The menu rebuilds on every agent status flip, re-rasterizing a dot per
    // tab each time — on the main thread. There are only a handful of distinct
    // (color, outline) dots, so rasterize each once and reuse the pixels.
    static RASTER_CACHE: LazyLock<Mutex<HashMap<(u8, u8, u8, bool), Vec<u8>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let rgba = {
        let mut cache = RASTER_CACHE.lock().unwrap_or_else(|err| err.into_inner());
        cache
            .entry((red, green, blue, outline))
            .or_insert_with(|| render_dot_rgba(red, green, blue, outline, SIZE))
            .clone()
    };
    tauri::image::Image::new_owned(rgba, SIZE, SIZE)
}

#[cfg(target_os = "macos")]
fn render_dot_rgba(red: u8, green: u8, blue: u8, outline: bool, size: u32) -> Vec<u8> {
    const SAMPLES: u32 = 4;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);

    for y in 0..size {
        for x in 0..size {
            let mut covered = 0u32;
            for sample_y in 0..SAMPLES {
                for sample_x in 0..SAMPLES {
                    let px = x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32;
                    let distance = ((px - 9.0).powi(2) + (py - 9.0).powi(2)).sqrt();
                    let inside = if outline {
                        (4.0..=5.6).contains(&distance)
                    } else {
                        distance <= 4.9
                    };
                    if inside {
                        covered += 1;
                    }
                }
            }
            let alpha = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }

    rgba
}

#[cfg(target_os = "macos")]
fn bento_icon() -> tauri::image::Image<'static> {
    const SIZE: u32 = 36;
    const SAMPLES: u32 = 4;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);

    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut covered = 0u32;
            for sample_y in 0..SAMPLES {
                for sample_x in 0..SAMPLES {
                    let px = x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32;
                    let py = y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32;
                    if bento_covers(px, py) {
                        covered += 1;
                    }
                }
            }
            let alpha = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }

    tauri::image::Image::new_owned(rgba, SIZE, SIZE)
}

// A 2x2 pane grid with the bottom-right cell swapped for a disc: three rounded
// squares plus a circle, drawn on a 36px canvas (24px grid scaled by 1.5).
#[cfg(target_os = "macos")]
fn bento_covers(x: f32, y: f32) -> bool {
    const CELL: f32 = 11.25;
    const NEAR: f32 = 5.25;
    const FAR: f32 = 19.5;
    const RADIUS: f32 = 2.625;

    let squares = [(NEAR, NEAR), (FAR, NEAR), (NEAR, FAR)];
    if squares
        .iter()
        .any(|&(left, top)| rounded_rect_covers(x, y, left, top, CELL, RADIUS))
    {
        return true;
    }

    (x - 25.125).powi(2) + (y - 25.125).powi(2) <= 5.625f32.powi(2)
}

#[cfg(target_os = "macos")]
fn rounded_rect_covers(x: f32, y: f32, left: f32, top: f32, size: f32, radius: f32) -> bool {
    let half = size / 2.0;
    let dx = ((x - (left + half)).abs() - (half - radius)).max(0.0);
    let dy = ((y - (top + half)).abs() - (half - radius)).max(0.0);
    dx * dx + dy * dy <= radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(title: &str, path: Option<&str>, status: Option<&str>, selected: bool) -> MenuBarTab {
        MenuBarTab {
            pane_id: "pane-1".to_string(),
            title: title.to_string(),
            path: path.map(str::to_string),
            status_tone: "idle".to_string(),
            status_label: status.map(str::to_string),
            waiting_on_pane: false,
            selected,
        }
    }

    #[test]
    fn tab_title_under_the_cap_is_kept() {
        let title = "a".repeat(MAX_TAB_TITLE_CHARS);
        assert_eq!(tab_menu_label(&tab(&title, None, None, false)), title);
    }

    #[test]
    fn tab_title_over_the_cap_is_truncated() {
        let title = "a".repeat(MAX_TAB_TITLE_CHARS + 8);
        let expected = format!("{}...", "a".repeat(MAX_TAB_TITLE_CHARS));
        assert_eq!(tab_menu_label(&tab(&title, None, None, false)), expected);
    }

    #[test]
    fn tab_title_cap_is_in_characters() {
        let title = "é".repeat(MAX_TAB_TITLE_CHARS + 1);
        let expected = format!("{}...", "é".repeat(MAX_TAB_TITLE_CHARS));
        assert_eq!(tab_menu_label(&tab(&title, None, None, false)), expected);
    }

    #[test]
    fn truncated_title_still_appends_path_and_status() {
        let title = "a".repeat(MAX_TAB_TITLE_CHARS + 4);
        let label = tab_menu_label(&tab(&title, Some("src/lib"), Some("working"), true));
        assert_eq!(
            label,
            format!("* {}... - src/lib (working)", "a".repeat(MAX_TAB_TITLE_CHARS))
        );
    }
}
