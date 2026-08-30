//! Paired-device records, endpoint identity, and revocation.
//!
//! A device is trusted with the whole app or it is not on this list at all
//! (docs/remote-control-plan.md): the records here are the entire
//! authorization model. Deleting one is complete revocation — the next
//! handshake fails at the accept gate, before any frame is read.

use crate::remote::endpoint::{DeviceGate, RemoteAccess};
use crate::state::AppState;
use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// One paired device, persisted in the owner-only preferences file. The
/// endpoint id is stored in its canonical display form and compared as a
/// string, so storage and the gate can never disagree on parsing.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemotePairedDevice {
    pub endpoint_id: String,
    pub name: String,
    pub paired_at: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u128>,
    #[serde(default)]
    pub read_only: bool,
}

pub fn list(state: &AppState) -> Vec<RemotePairedDevice> {
    crate::persistence::load_preferences(&state.config().workspace_root)
        .ok()
        .and_then(|prefs| prefs.remote_devices)
        .unwrap_or_default()
}

/// Adds (or re-pairs, replacing) a device record.
pub fn add(state: &AppState, device: RemotePairedDevice) -> Result<(), String> {
    crate::persistence::update_preferences(&state.config().workspace_root, |prefs| {
        let mut devices = prefs.remote_devices.take().unwrap_or_default();
        devices.retain(|existing| existing.endpoint_id != device.endpoint_id);
        devices.push(device);
        prefs.remote_devices = Some(devices);
    })
}

/// Removes a device. Returns whether it existed. The caller is responsible
/// for also disconnecting any live session (`RemoteControlRuntime::
/// disconnect_device`) — the store cannot reach the transport.
pub fn revoke(state: &AppState, endpoint_id: &str) -> Result<bool, String> {
    let mut existed = false;
    crate::persistence::update_preferences(&state.config().workspace_root, |prefs| {
        let mut devices = prefs.remote_devices.take().unwrap_or_default();
        let before = devices.len();
        devices.retain(|device| device.endpoint_id != endpoint_id);
        existed = devices.len() != before;
        prefs.remote_devices = Some(devices);
    })?;
    Ok(existed)
}

pub fn set_read_only(state: &AppState, endpoint_id: &str, read_only: bool) -> Result<bool, String> {
    let mut found = false;
    crate::persistence::update_preferences(&state.config().workspace_root, |prefs| {
        let mut devices = prefs.remote_devices.take().unwrap_or_default();
        for device in devices.iter_mut() {
            if device.endpoint_id == endpoint_id {
                device.read_only = read_only;
                found = true;
            }
        }
        prefs.remote_devices = Some(devices);
    })?;
    Ok(found)
}

fn touch_last_seen(state: &AppState, endpoint_id: &str) {
    let _ = crate::persistence::update_preferences(&state.config().workspace_root, |prefs| {
        let mut devices = prefs.remote_devices.take().unwrap_or_default();
        for device in devices.iter_mut() {
            if device.endpoint_id == endpoint_id {
                device.last_seen = Some(now_millis());
            }
        }
        prefs.remote_devices = Some(devices);
    });
}

/// The accept gate backed by the persisted list. Consulted per connection,
/// so a revocation applies to the very next handshake with no restart.
pub fn gate(state: AppState) -> DeviceGate {
    Arc::new(move |remote| {
        let id = remote.to_string();
        let device = list(&state)
            .into_iter()
            .find(|device| device.endpoint_id == id)?;
        touch_last_seen(&state, &id);
        Some(RemoteAccess {
            device_name: device.name,
            read_only: device.read_only,
        })
    })
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

/// Loads the endpoint's durable secret key, creating one on first use.
///
/// On macOS the key lives in the login Keychain (the same pattern
/// `publishing.rs` uses for the GitHub token). Elsewhere — dev builds,
/// tests, the future Linux port — it is an owner-only file beside the
/// preferences, which is also what makes this testable off-macOS.
pub fn load_or_create_identity(state: &AppState) -> Result<SecretKey, String> {
    #[cfg(target_os = "macos")]
    {
        keychain_identity()
    }
    #[cfg(not(target_os = "macos"))]
    {
        file_identity(&state.config().workspace_root)
    }
}

#[cfg(target_os = "macos")]
fn keychain_identity() -> Result<SecretKey, String> {
    const SERVICE: &str = "app.qmux.remote-control";
    const ACCOUNT: &str = "endpoint-key";
    let entry = keyring::Entry::new(SERVICE, ACCOUNT)
        .map_err(|err| format!("failed to open the remote-control keychain entry: {err}"))?;
    match entry.get_password() {
        Ok(hex) => decode_key(&hex),
        Err(keyring::Error::NoEntry) => {
            let key = SecretKey::generate();
            entry
                .set_password(&encode_key(&key))
                .map_err(|err| format!("failed to store the remote-control key: {err}"))?;
            Ok(key)
        }
        Err(err) => Err(format!("failed to read the remote-control key: {err}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn file_identity(workspace_root: &std::path::Path) -> Result<SecretKey, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = workspace_root.join("remote-identity");
    match std::fs::read_to_string(&path) {
        Ok(hex) => decode_key(hex.trim()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
            }
            crate::persistence::write_synced(&path, encode_key(&key).as_bytes())
                .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|err| format!("failed to restrict {}: {err}", path.display()))?;
            Ok(key)
        }
        Err(err) => Err(format!("failed to read {}: {err}", path.display())),
    }
}

fn encode_key(key: &SecretKey) -> String {
    key.to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_key(hex: &str) -> Result<SecretKey, String> {
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("stored remote-control key is malformed".to_string());
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        bytes[index] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
            .map_err(|err| format!("stored remote-control key is malformed: {err}"))?;
    }
    Ok(SecretKey::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support;
    use std::path::PathBuf;

    fn state(name: &str) -> AppState {
        let root = PathBuf::from(format!(
            "/tmp/qmux-remote-devices-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        AppState::new(test_support::config(root))
    }

    #[test]
    fn records_round_trip_revoke_and_read_only() {
        let state = state("crud");
        assert!(list(&state).is_empty());
        add(
            &state,
            RemotePairedDevice {
                endpoint_id: "device-a".to_string(),
                name: "Ray's iPhone".to_string(),
                paired_at: 1,
                last_seen: None,
                read_only: false,
            },
        )
        .unwrap();
        // Re-pairing the same endpoint replaces rather than duplicates.
        add(
            &state,
            RemotePairedDevice {
                endpoint_id: "device-a".to_string(),
                name: "Ray's iPhone (new)".to_string(),
                paired_at: 2,
                last_seen: None,
                read_only: false,
            },
        )
        .unwrap();
        let devices = list(&state);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Ray's iPhone (new)");

        assert!(set_read_only(&state, "device-a", true).unwrap());
        assert!(list(&state)[0].read_only);
        assert!(!set_read_only(&state, "missing", true).unwrap());

        assert!(revoke(&state, "device-a").unwrap());
        assert!(!revoke(&state, "device-a").unwrap());
        assert!(list(&state).is_empty());
    }

    #[test]
    fn the_gate_admits_only_listed_ids_and_touches_last_seen() {
        let state = state("gate");
        let key = SecretKey::generate();
        add(
            &state,
            RemotePairedDevice {
                endpoint_id: key.public().to_string(),
                name: "iPad".to_string(),
                paired_at: 1,
                last_seen: None,
                read_only: true,
            },
        )
        .unwrap();
        let gate = gate(state.clone());
        let access = gate(&key.public()).expect("listed device passes");
        assert_eq!(access.device_name, "iPad");
        assert!(access.read_only);
        assert!(
            list(&state)[0].last_seen.is_some(),
            "a successful gate pass records last_seen"
        );
        assert!(gate(&SecretKey::generate().public()).is_none());
    }

    #[test]
    fn identity_is_created_once_and_round_trips() {
        let state = state("identity");
        let first = load_or_create_identity(&state).unwrap();
        let second = load_or_create_identity(&state).unwrap();
        assert_eq!(first.to_bytes(), second.to_bytes());
        #[cfg(not(target_os = "macos"))]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = state.config().workspace_root.join("remote-identity");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be owner-only");
        }
    }
}
