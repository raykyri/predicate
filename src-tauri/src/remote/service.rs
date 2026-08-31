//! Lifecycle and status for remote control: the backend behind the toggle.
//!
//! Off means absent — `stop` shuts the runtime down and drops it, so no
//! endpoint is bound, nothing is advertised, and no relay connection exists.
//! The toggle's durable state lives in the owner-only preferences file (the
//! listener is backend state, and "on at launch" must be readable before
//! the webview exists).

use crate::remote::devices::{self, RemotePairedDevice};
use crate::remote::endpoint::{PairingInvite, RemoteControlRuntime, RemoteReach};
use crate::remote::pairing::PendingPairInfo;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// How far the endpoint reaches, as persisted and shown in the UI.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReachPref {
    /// Relays disabled, mDNS only: this network, invisible beyond it.
    #[default]
    Local,
    /// n0's relays plus discovery publishing: a separately confirmed consent.
    Anywhere,
}

impl From<ReachPref> for RemoteReach {
    fn from(pref: ReachPref) -> Self {
        match pref {
            ReachPref::Local => RemoteReach::Local,
            ReachPref::Anywhere => RemoteReach::Anywhere,
        }
    }
}

/// The toggle's durable half, in `AppPreferences`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteControlPrefs {
    #[serde(default)]
    pub launch_enabled: bool,
    #[serde(default)]
    pub reach: ReachPref,
}

fn prefs(state: &AppState) -> RemoteControlPrefs {
    crate::persistence::load_preferences(&state.config().workspace_root)
        .ok()
        .and_then(|prefs| prefs.remote_control)
        .unwrap_or_default()
}

fn update_prefs(
    state: &AppState,
    mutate: impl FnOnce(&mut RemoteControlPrefs),
) -> Result<(), String> {
    crate::persistence::update_preferences(&state.config().workspace_root, |preferences| {
        let mut current = preferences.remote_control.unwrap_or_default();
        mutate(&mut current);
        preferences.remote_control = Some(current);
    })
}

/// Everything the popover renders.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub enabled: bool,
    pub reach: ReachPref,
    pub launch_enabled: bool,
    /// This Mac's endpoint id while enabled.
    pub endpoint_id: Option<String>,
    pub devices: Vec<RemoteDeviceStatus>,
    pub sessions: Vec<RemoteSessionStatus>,
    pub pending_pair: Option<PendingPairInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDeviceStatus {
    #[serde(flatten)]
    pub device: RemotePairedDevice,
    pub connected: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionStatus {
    pub endpoint_id: String,
    pub device_name: String,
    pub connected_at: u128,
}

pub fn status(state: &AppState) -> RemoteStatus {
    let prefs = prefs(state);
    let runtime = state.remote_runtime();
    let sessions: Vec<RemoteSessionStatus> = runtime
        .as_ref()
        .map(|runtime| {
            runtime
                .sessions()
                .into_iter()
                .map(|entry| RemoteSessionStatus {
                    endpoint_id: entry.endpoint_id.to_string(),
                    device_name: entry.device_name,
                    connected_at: entry.connected_at,
                })
                .collect()
        })
        .unwrap_or_default();
    let devices = devices::list(state)
        .into_iter()
        .map(|device| {
            let connected = sessions
                .iter()
                .any(|session| session.endpoint_id == device.endpoint_id);
            RemoteDeviceStatus { device, connected }
        })
        .collect();
    RemoteStatus {
        enabled: runtime.is_some(),
        reach: prefs.reach,
        launch_enabled: prefs.launch_enabled,
        endpoint_id: runtime
            .as_ref()
            .map(|runtime| runtime.endpoint_id().to_string()),
        devices,
        sessions,
        pending_pair: runtime.as_ref().and_then(|runtime| runtime.pending_pair()),
    }
}

fn emit_status(state: &AppState) {
    let snapshot = status(state);
    state.emit(crate::events::QmuxEvent::new(
        "remote.status_changed",
        None,
        None,
        serde_json::to_value(&snapshot).unwrap_or_default(),
    ));
}

/// Turns remote control on with the persisted reach. Idempotent.
pub fn start(state: &AppState) -> Result<RemoteStatus, String> {
    state.with_remote_lifecycle(|| start_locked(state))
}

fn start_locked(state: &AppState) -> Result<RemoteStatus, String> {
    if state.remote_runtime().is_some() {
        return Ok(status(state));
    }
    let secret = devices::load_or_create_identity(state)?;
    let reach = prefs(state).reach;
    let runtime = RemoteControlRuntime::start(
        state.clone(),
        secret,
        reach.into(),
        true,
        devices::gate(state.clone()),
    )?;
    if let Some(displaced) = state.set_remote_runtime(Some(runtime)) {
        // The lifecycle lock makes this unreachable in normal operation, but
        // never abandon a bound endpoint if replacement semantics change.
        displaced.shutdown();
    }
    emit_status(state);
    Ok(status(state))
}

/// Turns remote control off: closes every session, unbinds the endpoint,
/// withdraws discovery, drops the relay connection, drops the runtime.
pub fn stop(state: &AppState) -> RemoteStatus {
    state.with_remote_lifecycle(|| stop_locked(state))
}

fn stop_locked(state: &AppState) -> RemoteStatus {
    if let Some(runtime) = state.set_remote_runtime(None) {
        runtime.shutdown();
        emit_status(state);
    }
    status(state)
}

fn require_runtime(state: &AppState) -> Result<Arc<RemoteControlRuntime>, String> {
    state
        .remote_runtime()
        .ok_or_else(|| "remote control is off".to_string())
}

#[tauri::command(async)]
pub fn remote_status_get(state: tauri::State<'_, AppState>) -> Result<RemoteStatus, String> {
    Ok(status(&state))
}

#[tauri::command(async)]
pub fn remote_set_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<RemoteStatus, String> {
    if enabled {
        start(&state)
    } else {
        Ok(stop(&state))
    }
}

#[tauri::command(async)]
pub fn remote_set_reach(
    state: tauri::State<'_, AppState>,
    reach: ReachPref,
) -> Result<RemoteStatus, String> {
    state.with_remote_lifecycle(|| {
        update_prefs(&state, |prefs| prefs.reach = reach)?;
        if state.remote_runtime().is_some() {
            // The reach is endpoint construction, not a runtime flag: rebind.
            stop_locked(&state);
            return start_locked(&state);
        }
        emit_status(&state);
        Ok(status(&state))
    })
}

#[tauri::command(async)]
pub fn remote_set_launch_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<RemoteStatus, String> {
    update_prefs(&state, |prefs| prefs.launch_enabled = enabled)?;
    emit_status(&state);
    Ok(status(&state))
}

/// What the pairing panel shows: the invite plus a rendered QR.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingPanel {
    #[serde(flatten)]
    pub invite: PairingInvite,
    pub qr_svg: String,
}

#[tauri::command(async)]
pub fn remote_pairing_begin(state: tauri::State<'_, AppState>) -> Result<PairingPanel, String> {
    let runtime = require_runtime(&state)?;
    let invite = runtime.begin_pairing()?;
    let qr_svg = render_qr_svg(&invite.payload)?;
    emit_status(&state);
    Ok(PairingPanel { invite, qr_svg })
}

#[tauri::command(async)]
pub fn remote_pairing_cancel(state: tauri::State<'_, AppState>) -> Result<RemoteStatus, String> {
    if let Some(runtime) = state.remote_runtime() {
        runtime.cancel_pairing();
    }
    emit_status(&state);
    Ok(status(&state))
}

#[tauri::command(async)]
pub fn remote_pair_respond(
    state: tauri::State<'_, AppState>,
    request_id: String,
    approved: bool,
    read_only: bool,
) -> Result<(), String> {
    require_runtime(&state)?.respond_pair(&request_id, approved, read_only)?;
    // The pairing task still has to persist the device before its status is
    // authoritative. It emits pair_resolved after doing so; returning a
    // snapshot here would race that write and could erase the new device in
    // the UI.
    Ok(())
}

#[tauri::command(async)]
pub fn remote_device_revoke(
    state: tauri::State<'_, AppState>,
    endpoint_id: String,
) -> Result<RemoteStatus, String> {
    devices::revoke(&state, &endpoint_id)?;
    if let Some(runtime) = state.remote_runtime() {
        runtime.disconnect_device(&endpoint_id);
    }
    emit_status(&state);
    Ok(status(&state))
}

#[tauri::command(async)]
pub fn remote_device_set_read_only(
    state: tauri::State<'_, AppState>,
    endpoint_id: String,
    read_only: bool,
) -> Result<RemoteStatus, String> {
    if !devices::set_read_only(&state, &endpoint_id, read_only)? {
        return Err("that device is not paired".to_string());
    }
    // A live session keeps the permission it connected with; the flag
    // applies from its next connection.
    if let Some(runtime) = state.remote_runtime() {
        runtime.disconnect_device(&endpoint_id);
    }
    emit_status(&state);
    Ok(status(&state))
}

fn render_qr_svg(payload: &str) -> Result<String, String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(payload.as_bytes())
        .map_err(|err| format!("failed to build the pairing QR: {err}"))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support;
    use std::path::PathBuf;

    fn state(name: &str) -> AppState {
        let root = PathBuf::from(format!(
            "/tmp/qmux-remote-service-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        AppState::new(test_support::config(root))
    }

    #[test]
    fn the_toggle_binds_and_provably_unbinds() {
        let _serial = test_support::net_serial_guard();
        let state = state("toggle");
        assert!(!status(&state).enabled);

        let started = start(&state).unwrap();
        assert!(started.enabled);
        let endpoint_id = started.endpoint_id.clone().expect("an id while enabled");
        // Idempotent, and the identity is durable.
        let again = start(&state).unwrap();
        assert_eq!(again.endpoint_id.as_ref(), Some(&endpoint_id));

        let stopped = stop(&state);
        assert!(!stopped.enabled);
        assert!(stopped.endpoint_id.is_none());
        assert!(state.remote_runtime().is_none(), "off means absent");

        // The same key comes back on the next start.
        let restarted = start(&state).unwrap();
        assert_eq!(restarted.endpoint_id.as_ref(), Some(&endpoint_id));
        stop(&state);
    }

    #[test]
    fn reach_persists_and_a_pairing_panel_needs_the_runtime() {
        let _serial = test_support::net_serial_guard();
        let state = state("reach");
        update_prefs(&state, |prefs| prefs.reach = ReachPref::Anywhere).unwrap();
        assert_eq!(status(&state).reach, ReachPref::Anywhere);
        update_prefs(&state, |prefs| prefs.reach = ReachPref::Local).unwrap();

        // Pairing is meaningless while off.
        assert!(state.remote_runtime().is_none());
        let runtime_error = require_runtime(&state).err().unwrap();
        assert!(runtime_error.contains("off"));

        start(&state).unwrap();
        let runtime = state.remote_runtime().unwrap();
        let invite = runtime.begin_pairing().unwrap();
        let svg = render_qr_svg(&invite.payload).unwrap();
        assert!(svg.contains("<svg"), "expected an svg, got: {svg:.>40}");
        stop(&state);
    }

    #[test]
    fn lifecycle_calls_are_safe_inside_a_tokio_runtime() {
        let _serial = test_support::net_serial_guard();
        let state = state("nested-runtime");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                assert!(start(&state).unwrap().enabled);
                assert!(!stop(&state).enabled);
            });
    }

    #[test]
    fn concurrent_starts_install_exactly_one_endpoint() {
        let _serial = test_support::net_serial_guard();
        let state = state("concurrent-start");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    start(&state).unwrap().endpoint_id.unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let endpoint_ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(endpoint_ids[0], endpoint_ids[1]);
        assert_eq!(
            state.remote_runtime().unwrap().endpoint_id().to_string(),
            endpoint_ids[0]
        );
        stop(&state);
    }
}
