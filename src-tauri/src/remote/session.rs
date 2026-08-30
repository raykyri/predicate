//! Per-connection remote session state and, in stage 2, the frame loop.
//!
//! [`RemoteSession`] is deliberately transport-free so `control.rs` can be
//! exercised against it with no endpoint bound (stage 1); the stage-2 frame
//! loop drives the same struct.

use std::sync::Mutex;

/// State one connected device carries across calls.
#[derive(Debug)]
pub struct RemoteSession {
    /// Human name shown in the sessions list, e.g. "Ray's iPhone".
    pub device_name: String,
    /// Devices paired read-only may look at everything and change nothing.
    pub read_only: bool,
    /// The pane this session's context resolves against when an operation
    /// needs "the current pane". Set by the `session.focus` operation; falls
    /// back to the app's active pane while unset or stale.
    focus_pane: Mutex<Option<String>>,
}

impl RemoteSession {
    pub fn new(device_name: impl Into<String>, read_only: bool) -> Self {
        Self {
            device_name: device_name.into(),
            read_only,
            focus_pane: Mutex::new(None),
        }
    }

    pub fn focus_pane(&self) -> Option<String> {
        self.focus_pane.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn set_focus_pane(&self, pane_id: String) {
        if let Ok(mut guard) = self.focus_pane.lock() {
            *guard = Some(pane_id);
        }
    }
}
