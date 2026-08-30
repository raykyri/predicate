//! Event fan-out and per-pane byte rings for remote sessions.
//!
//! The producers — `AppState::emit` and the PTY reader thread — are on the
//! critical path for the person sitting at the Mac, so both taps are
//! non-blocking by construction: a cheap atomic check when no session is
//! subscribed, and short lock holds that only move bytes between buffers
//! when one is. A slow phone loses data, never the Mac.
//!
//! Recovery is always "re-read the truth", never "replay the difference":
//!
//! - A pane ring that overflows drops its oldest bytes and marks a gap; the
//!   pump then sends a reset marker and a fresh full replay from the durable
//!   scrollback journal. A terminal tolerates this because a fresh screen is
//!   always a valid resync point.
//! - The event queue never silently drops: on overflow the session is marked
//!   desynced, queueing stops, and exactly one `Resync` frame goes out; the
//!   client refetches pane/agent/queue state. A lost `turn.updated` would
//!   leave the phone quietly wrong, which is worse than a visible resync.

use crate::events::QmuxEvent;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Retained bytes per subscribed pane per session (the size a drained pump
/// can fall behind before the ring gaps).
pub const PANE_RING_CAP: usize = 256 * 1024;
/// Queued events per session before it is marked desynced.
pub const EVENT_QUEUE_CAP: usize = 1024;

/// One subscribed pane's pending bytes.
#[derive(Default)]
pub struct PaneRing {
    data: VecDeque<u8>,
    gap: bool,
}

impl PaneRing {
    /// Non-blocking push: on overflow the oldest bytes are dropped and the
    /// gap flag set, so the producer never waits on a slow consumer.
    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= PANE_RING_CAP {
            // The chunk alone floods the ring: keep only its tail.
            self.data.clear();
            self.data
                .extend(&chunk[chunk.len() - (PANE_RING_CAP - 1)..]);
            self.gap = true;
            return;
        }
        let overflow = (self.data.len() + chunk.len()).saturating_sub(PANE_RING_CAP);
        if overflow > 0 {
            self.data.drain(..overflow);
            self.gap = true;
        }
        self.data.extend(chunk);
    }

    /// Takes everything pending. `gapped` tells the pump to reset the client
    /// and re-prime from the journal instead of forwarding the (incomplete)
    /// buffered bytes.
    pub fn drain(&mut self) -> (Vec<u8>, bool) {
        let gapped = self.gap;
        self.gap = false;
        let data = self.data.drain(..).collect();
        (data, gapped)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty() && !self.gap
    }
}

/// One subscribed pane's ring plus its pump's wakeup. Each pane stream has
/// its own pump task, so each needs its own `Notify` — a shared one with
/// `notify_one` would wake an arbitrary pump and strand the others.
pub struct PaneChannel {
    ring: Mutex<PaneRing>,
    pub notify: tokio::sync::Notify,
}

impl PaneChannel {
    fn push(&self, chunk: &[u8]) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.push(chunk);
        }
        self.notify.notify_one();
    }

    /// See [`PaneRing::drain`].
    pub fn drain(&self) -> (Vec<u8>, bool) {
        self.ring
            .lock()
            .map(|mut ring| ring.drain())
            .unwrap_or_default()
    }

    /// Discards everything pending (used before a gap re-prime, where the
    /// journal replay supersedes the buffered fragment).
    pub fn discard(&self) {
        if let Ok(mut ring) = self.ring.lock() {
            let _ = ring.drain();
        }
    }
}

/// The queues one remote session drains.
pub struct SessionChannels {
    events_on: AtomicBool,
    events: Mutex<VecDeque<Value>>,
    /// Set when the event queue overflowed; cleared when the pump has sent
    /// its one `Resync`. While set, nothing queues.
    desynced: AtomicBool,
    panes: Mutex<HashMap<String, Arc<PaneChannel>>>,
    /// Wakes the session's event pump.
    pub events_notify: tokio::sync::Notify,
}

impl SessionChannels {
    fn new() -> Self {
        Self {
            events_on: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
            desynced: AtomicBool::new(false),
            panes: Mutex::new(HashMap::new()),
            events_notify: tokio::sync::Notify::new(),
        }
    }

    pub fn set_events_on(&self, on: bool) {
        self.events_on.store(on, Ordering::SeqCst);
    }

    /// Registers a pane's channel, returning it for the pump. Replaces any
    /// previous channel for the pane (a re-subscribe starts clean).
    pub fn register_pane(&self, pane_id: &str) -> Arc<PaneChannel> {
        let channel = Arc::new(PaneChannel {
            ring: Mutex::new(PaneRing::default()),
            notify: tokio::sync::Notify::new(),
        });
        if let Ok(mut panes) = self.panes.lock() {
            panes.insert(pane_id.to_string(), channel.clone());
        }
        channel
    }

    pub fn unregister_pane(&self, pane_id: &str) {
        if let Ok(mut panes) = self.panes.lock() {
            panes.remove(pane_id);
        }
    }

    pub fn subscribed_panes(&self) -> Vec<String> {
        self.panes
            .lock()
            .map(|panes| panes.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn push_event(&self, event: &Value) {
        if !self.events_on.load(Ordering::SeqCst) || self.desynced.load(Ordering::SeqCst) {
            return;
        }
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        if events.len() >= EVENT_QUEUE_CAP {
            events.clear();
            self.desynced.store(true, Ordering::SeqCst);
        } else {
            events.push_back(event.clone());
        }
    }

    fn push_pane_bytes(&self, pane_id: &str, chunk: &[u8]) {
        let channel = self
            .panes
            .lock()
            .ok()
            .and_then(|panes| panes.get(pane_id).cloned());
        if let Some(channel) = channel {
            channel.push(chunk);
        }
    }

    /// Drains pending events for the pump. `resync` means the queue
    /// overflowed since the last drain: the pump must send exactly one
    /// `Resync` (any drained events predate the loss and are returned
    /// empty), after which queueing resumes.
    pub fn drain_events(&self) -> (Vec<Value>, bool) {
        if self.desynced.swap(false, Ordering::SeqCst) {
            if let Ok(mut events) = self.events.lock() {
                events.clear();
            }
            return (Vec::new(), true);
        }
        let events = self
            .events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default();
        (events, false)
    }
}

/// The registry every producer publishes through. One per `AppState`,
/// constructed dormant; `active` keeps the no-session fast path to a single
/// atomic load.
pub struct RemoteFanout {
    sessions: Mutex<HashMap<u64, Arc<SessionChannels>>>,
    next_id: AtomicU64,
    active: AtomicUsize,
}

impl Default for RemoteFanout {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            active: AtomicUsize::new(0),
        }
    }
}

impl RemoteFanout {
    pub fn register_session(&self) -> (u64, Arc<SessionChannels>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let channels = Arc::new(SessionChannels::new());
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(id, channels.clone());
            self.active.store(sessions.len(), Ordering::SeqCst);
        }
        (id, channels)
    }

    pub fn unregister_session(&self, id: u64) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(&id);
            self.active.store(sessions.len(), Ordering::SeqCst);
        }
    }

    pub fn session_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Called from `AppState::emit` for every event. Serializes at most once,
    /// and only when a session is live.
    pub fn publish_event(&self, event: &QmuxEvent) {
        if self.active.load(Ordering::SeqCst) == 0 {
            return;
        }
        let Ok(value) = serde_json::to_value(event) else {
            return;
        };
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        for channels in sessions.values() {
            channels.push_event(&value);
            channels.events_notify.notify_one();
        }
    }

    /// Called from the PTY reader thread for every live chunk.
    pub fn publish_pane_bytes(&self, pane_id: &str, chunk: &[u8]) {
        if self.active.load(Ordering::SeqCst) == 0 {
            return;
        }
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        for channels in sessions.values() {
            channels.push_pane_bytes(pane_id, chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pane_ring_drops_oldest_and_marks_the_gap() {
        let mut ring = PaneRing::default();
        ring.push(b"hello");
        let (data, gapped) = ring.drain();
        assert_eq!(data, b"hello");
        assert!(!gapped);

        // Overflow: the ring keeps the newest bytes and marks the gap.
        let big = vec![b'x'; PANE_RING_CAP - 2];
        ring.push(&big);
        ring.push(b"tail");
        let (data, gapped) = ring.drain();
        assert!(gapped, "overflow must set the gap flag");
        assert_eq!(data.len(), PANE_RING_CAP);
        assert!(data.ends_with(b"tail"), "newest bytes are retained");

        // A single chunk larger than the ring keeps only its tail.
        let flood = vec![b'y'; PANE_RING_CAP * 2];
        ring.push(&flood);
        let (data, gapped) = ring.drain();
        assert!(gapped);
        assert_eq!(data.len(), PANE_RING_CAP - 1);
    }

    #[test]
    fn event_overflow_desyncs_once_and_resumes_after_resync() {
        let channels = SessionChannels::new();
        channels.set_events_on(true);
        let event = json!({ "type": "turn.updated" });
        for _ in 0..EVENT_QUEUE_CAP {
            channels.push_event(&event);
        }
        // The push that overflows clears the queue and stops queueing.
        channels.push_event(&event);
        channels.push_event(&event);
        let (events, resync) = channels.drain_events();
        assert!(resync, "overflow must surface as a resync");
        assert!(
            events.is_empty(),
            "events from before the loss must not be replayed"
        );
        // After the resync drain, queueing resumes.
        channels.push_event(&event);
        let (events, resync) = channels.drain_events();
        assert!(!resync);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn fanout_is_dormant_with_no_sessions_and_scoped_per_pane() {
        let fanout = RemoteFanout::default();
        assert_eq!(fanout.session_count(), 0);
        // No sessions: publishing is a no-op (and must not panic).
        fanout.publish_pane_bytes("pane-1", b"bytes");

        let (id, channels) = fanout.register_session();
        channels.set_events_on(true);
        let channel = channels.register_pane("pane-1");
        fanout.publish_pane_bytes("pane-1", b"bytes");
        fanout.publish_pane_bytes("pane-2", b"other pane");
        let (data, gapped) = channel.drain();
        assert_eq!(data, b"bytes", "only the subscribed pane's bytes arrive");
        assert!(!gapped);

        fanout.publish_event(&QmuxEvent::new("agent.status", None, None, json!({})));
        let (events, resync) = channels.drain_events();
        assert_eq!(events.len(), 1);
        assert!(!resync);

        fanout.unregister_session(id);
        assert_eq!(fanout.session_count(), 0);
    }
}
