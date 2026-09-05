use crate::state::{SharedChild, SharedMaster, SharedWriter};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// Maximum overlap retained locally for reconciling successive tmux history
/// captures. The remote pane may keep substantially more history; qmux only
/// needs a distinctive tail of the last accepted capture to find where new
/// scrolled-off lines begin.
const HISTORY_CHECKPOINT_CAP: usize = 256 * 1024;

#[derive(Default)]
pub struct RemoteHistoryCheckpoint {
    accepted_tail: Mutex<Vec<u8>>,
}

impl RemoteHistoryCheckpoint {
    pub fn new(accepted_tail: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            accepted_tail: Mutex::new(checkpoint_tail(&accepted_tail)),
        })
    }

    /// Returns only bytes following the most recent accepted overlap.
    ///
    /// tmux history is append-only until its configured limit rolls over. The
    /// full checkpoint handles the common case; progressively shorter suffixes
    /// retain continuity when rollover removes the oldest part of the saved
    /// tail. If there is no trustworthy overlap, returning the whole capture
    /// favors visible continuity over silently dropping remote output.
    pub fn delta(&self, capture: &[u8]) -> Vec<u8> {
        let accepted = self
            .accepted_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        history_delta(&accepted, capture).to_vec()
    }

    pub fn advance(&self, capture: &[u8]) -> Vec<u8> {
        let tail = checkpoint_tail(capture);
        *self
            .accepted_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = tail.clone();
        tail
    }

    pub fn has_trustworthy_overlap(&self, capture: &[u8]) -> bool {
        let accepted = self
            .accepted_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        accepted.is_empty() || overlap_end(&accepted, capture).is_some()
    }
}

fn checkpoint_tail(capture: &[u8]) -> Vec<u8> {
    if capture.len() <= HISTORY_CHECKPOINT_CAP {
        return capture.to_vec();
    }
    let raw_start = capture.len() - HISTORY_CHECKPOINT_CAP;
    let start = capture[raw_start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(raw_start, |offset| raw_start + offset + 1);
    capture[start..].to_vec()
}

fn history_delta<'a>(accepted: &[u8], capture: &'a [u8]) -> &'a [u8] {
    if capture.is_empty() {
        return capture;
    }
    if accepted.is_empty() {
        return capture;
    }

    overlap_end(accepted, capture)
        .map(|end| &capture[end..])
        .unwrap_or(capture)
}

fn overlap_end(accepted: &[u8], capture: &[u8]) -> Option<usize> {
    if accepted.is_empty() {
        return Some(0);
    }
    let mut overlap_lengths = vec![accepted.len()];
    for limit in [64 * 1024, 16 * 1024, 4 * 1024, 1024, 256] {
        if accepted.len() > limit {
            overlap_lengths.push(limit);
        }
    }
    let without_final_newline = accepted.strip_suffix(b"\n").unwrap_or(accepted);
    let last_line_start = without_final_newline
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let last_line_length = accepted.len() - last_line_start;
    let last_line_is_distinctive = accepted[last_line_start..]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace());
    if last_line_length >= 4
        && last_line_is_distinctive
        && !overlap_lengths.contains(&last_line_length)
    {
        overlap_lengths.push(last_line_length);
    }
    for length in overlap_lengths {
        let suffix = &accepted[accepted.len() - length..];
        if let Some(offset) = unique_subslice(capture, suffix) {
            return Some(offset + suffix.len());
        }
    }
    None
}

/// Linear-time last-substring search. Captures can be several megabytes and
/// terminal lines often share long runs of spaces, so repeated `windows` +
/// equality checks can otherwise turn reconnect into quadratic work.
fn unique_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }

    let mut matched = 0;
    let mut found = None;
    for (index, byte) in haystack.iter().enumerate() {
        while matched > 0 && *byte != needle[matched] {
            matched = prefix[matched - 1];
        }
        if *byte == needle[matched] {
            matched += 1;
        }
        if matched == needle.len() {
            let offset = index + 1 - needle.len();
            if found.replace(offset).is_some() {
                return None;
            }
            matched = prefix[matched - 1];
        }
    }
    found
}

/// One disposable local SSH/PTTY attachment to a durable remote tmux session.
#[derive(Clone)]
pub struct RemoteAttachment {
    pub child: SharedChild,
    pub master: SharedMaster,
    pub writer: SharedWriter,
}

#[derive(Default)]
struct AttachmentState {
    generation: u64,
    current: Option<RemoteAttachment>,
    client_pid: Option<u32>,
    confirmed: bool,
}

/// Stable process-local owner for a sequence of disposable SSH attachments.
///
/// Every asynchronous reader carries the generation returned by
/// `begin_generation`. It may mutate controller state only while that token is
/// still current, preventing a late EOF from attachment N from disconnecting
/// an already-installed attachment N+1.
#[derive(Default)]
pub struct RemoteAttachmentController {
    state: Mutex<AttachmentState>,
    reconnect_failures: AtomicU32,
    recovery: Mutex<RecoveryState>,
    changed: Condvar,
}

#[derive(Default)]
struct RecoveryState {
    running: bool,
    revision: u64,
    sleeping: bool,
    stopped: bool,
    reason: String,
}

impl RemoteAttachmentController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One worker owns all checks/reattachments. Requests interrupt its backoff
    /// and invalidate in-flight observations without creating another worker.
    pub fn request_recovery(&self, reason: &str) -> bool {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.stopped {
            return false;
        }
        recovery.revision += 1;
        if !recovery.running || reason != "connectionLost" {
            recovery.reason = reason.to_string();
        }
        let start = !recovery.running;
        recovery.running = true;
        self.changed.notify_all();
        start
    }

    pub fn cancel_recovery(&self) {
        let mut recovery = self.recovery.lock().unwrap();
        recovery.stopped = true;
        recovery.revision += 1;
        self.changed.notify_all();
    }

    pub fn set_sleeping(&self, sleeping: bool) {
        let mut recovery = self.recovery.lock().unwrap();
        recovery.sleeping = sleeping;
        recovery.revision += 1;
        self.changed.notify_all();
    }

    pub fn recovery_request(&self) -> (u64, bool, String) {
        let recovery = self.recovery.lock().unwrap();
        (
            recovery.revision,
            recovery.sleeping || recovery.stopped,
            recovery.reason.clone(),
        )
    }

    pub fn recovery_is_current(&self, revision: u64) -> bool {
        let recovery = self.recovery.lock().unwrap();
        recovery.revision == revision && !recovery.sleeping && !recovery.stopped
    }

    pub fn observe_recovery(&self, revision: u64, update: impl FnOnce()) -> bool {
        let recovery = self.recovery.lock().unwrap();
        if recovery.revision != revision || recovery.sleeping || recovery.stopped {
            return false;
        }
        update();
        true
    }

    pub fn finish_recovery(&self, revision: u64) -> bool {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.revision != revision {
            return false;
        }
        recovery.running = false;
        true
    }

    pub fn wait_for_recovery(&self, revision: u64, delay: Duration) {
        let recovery = self.recovery.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(recovery, delay, |r| r.revision == revision)
            .unwrap();
    }

    pub fn client_identity(&self) -> Option<(u64, Option<u32>)> {
        let state = self.state.lock().unwrap();
        state
            .current
            .as_ref()
            .map(|_| (state.generation, state.client_pid))
    }

    pub fn attachment_verified(&self) -> bool {
        self.state.lock().unwrap().confirmed
    }

    pub fn record_client_pid(&self, generation: u64, pid: u32) {
        let mut state = self.state.lock().unwrap();
        if state.generation == generation && state.current.is_some() {
            state.client_pid.get_or_insert(pid);
        }
    }

    /// Starts a new attachment attempt and removes the previous generation.
    /// The caller owns teardown of the returned attachment outside this lock.
    pub fn begin_generation(&self) -> (u64, Option<RemoteAttachment>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation = state
            .generation
            .checked_add(1)
            .expect("remote attachment generation exhausted");
        state.client_pid = None;
        state.confirmed = false;
        let previous = state.current.take();
        (state.generation, previous)
    }

    /// Carries reconnect backoff across short-lived SSH generations. A process
    /// that spawns successfully and exits before its tmux client is verified is
    /// still a failed attachment; resetting on spawn would otherwise create a
    /// permanent four-attempts-per-second loop.
    pub fn next_reconnect_delay(&self, initial: Duration, maximum: Duration) -> Duration {
        let failures = self.reconnect_failures.fetch_add(1, Ordering::Relaxed);
        let multiplier = 1_u32.checked_shl(failures.min(16)).unwrap_or(u32::MAX);
        initial
            .checked_mul(multiplier)
            .unwrap_or(maximum)
            .min(maximum)
    }

    /// Marks a generation live only while it still owns the controller. A
    /// superseded verifier can race its response with the next
    /// `begin_generation`; that response must not reset backoff or report the new
    /// generation as connected.
    pub fn mark_attachment_live_if_current(&self, generation: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != generation || state.current.is_none() {
            return false;
        }
        state.confirmed = true;
        self.reconnect_failures.store(0, Ordering::Relaxed);
        true
    }

    /// Installs an attachment only if no newer attempt has begun.
    pub fn install_if_current(
        &self,
        generation: u64,
        attachment: RemoteAttachment,
    ) -> Result<(), RemoteAttachment> {
        let recovery = self.recovery.lock().unwrap();
        if recovery.stopped {
            return Err(attachment);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != generation || state.current.is_some() {
            return Err(attachment);
        }
        state.current = Some(attachment);
        Ok(())
    }

    /// Clears and returns this generation's attachment. A stale EOF is a no-op.
    pub fn clear_if_current(&self, generation: u64) -> Option<RemoteAttachment> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.generation == generation)
            .then(|| state.current.take())
            .flatten()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_current(&self, generation: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation == generation && state.current.is_some()
    }

    pub fn current_master(&self) -> Option<SharedMaster> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .as_ref()
            .map(|attachment| attachment.master.clone())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn current_child(&self) -> Option<SharedChild> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .as_ref()
            .map(|attachment| attachment.child.clone())
    }

    fn current_writer(&self) -> Option<SharedWriter> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .as_ref()
            .map(|attachment| attachment.writer.clone())
    }

    /// A writer whose identity does not change across reconnects.
    pub fn stable_writer(self: &Arc<Self>) -> SharedWriter {
        Arc::new(Mutex::new(Box::new(RemoteInputWriter {
            controller: Arc::downgrade(self),
        })))
    }
}

struct RemoteInputWriter {
    controller: Weak<RemoteAttachmentController>,
}

impl RemoteInputWriter {
    fn writer(&self) -> io::Result<SharedWriter> {
        self.controller
            .upgrade()
            .and_then(|controller| controller.current_writer())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "remote pane is offline"))
    }
}

impl Write for RemoteInputWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let writer = self.writer()?;
        writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        let writer = self.writer()?;
        writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .flush()
    }
}

/// The remote launcher announces its PID before exec'ing tmux. Confirming that
/// exact PID in list-clients proves attachment; SSH banners/errors cannot do so.
#[derive(Default)]
pub struct RemoteClientHandshake {
    pending: Vec<u8>,
    complete: bool,
}

impl RemoteClientHandshake {
    pub fn feed(&mut self, bytes: &[u8]) -> (Vec<u8>, Option<u32>) {
        const PREFIX: &[u8] = b"\x1b]777;qmux-client-pid=";
        if self.complete {
            return (bytes.to_vec(), None);
        }
        self.pending.extend_from_slice(bytes);
        if let Some(start) = self.pending.windows(PREFIX.len()).position(|w| w == PREFIX) {
            let value_start = start + PREFIX.len();
            if let Some(end) = self.pending[value_start..].iter().position(|b| *b == 7) {
                let end = value_start + end;
                let pid = std::str::from_utf8(&self.pending[value_start..end])
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .filter(|pid| *pid > 0);
                if pid.is_some() {
                    let mut output = self.pending[..start].to_vec();
                    output.extend_from_slice(&self.pending[end + 1..]);
                    self.pending.clear();
                    self.complete = true;
                    return (output, pid);
                }
            } else if self.pending.len() - value_start <= 10 {
                let output = self.pending[..start].to_vec();
                self.pending.drain(..start);
                return (output, None);
            }
        }
        let keep = (1..PREFIX.len())
            .rev()
            .find(|n| self.pending.ends_with(&PREFIX[..*n]))
            .unwrap_or(0);
        let output = self.pending.drain(..self.pending.len() - keep).collect();
        (output, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};

    #[derive(Debug)]
    struct FakeChild;

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[derive(Clone)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn attachment(output: Arc<Mutex<Vec<u8>>>) -> RemoteAttachment {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        drop(pair.slave);
        RemoteAttachment {
            child: Arc::new(Mutex::new(Box::new(FakeChild))),
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(Box::new(RecordingWriter(output)))),
        }
    }

    #[test]
    fn handshake_handles_every_split_and_does_not_expose_control_metadata() {
        let wire = b"banner\r\n\x1b]777;qmux-client-pid=12345\x07prompt> ";
        for split in 0..=wire.len() {
            let mut handshake = RemoteClientHandshake::default();
            let (mut first, pid1) = handshake.feed(&wire[..split]);
            let (second, pid2) = handshake.feed(&wire[split..]);
            first.extend(second);
            assert_eq!(first, b"banner\r\nprompt> ");
            assert_eq!(pid1.or(pid2), Some(12345));
        }
        let mut handshake = RemoteClientHandshake::default();
        assert_eq!(
            handshake.feed(b"Permission denied\r\n"),
            (b"Permission denied\r\n".to_vec(), None)
        );
        assert_eq!(
            handshake.feed(b"\x1b]777;qmux-client-pid=oops\x07"),
            (b"\x1b]777;qmux-client-pid=oops\x07".to_vec(), None)
        );
    }

    #[test]
    fn recovery_requests_coalesce_and_sleep_fences_old_observations() {
        let controller = RemoteAttachmentController::new();
        assert!(controller.request_recovery("systemWake"));
        let (first, _, _) = controller.recovery_request();
        assert!(!controller.request_recovery("connectionLost"));
        let (second, _, reason) = controller.recovery_request();
        assert_eq!(reason, "systemWake");
        assert!(!controller.finish_recovery(first));
        assert!(!controller.observe_recovery(first, || panic!("stale observation accepted")));
        controller.set_sleeping(true);
        assert!(!controller.observe_recovery(second, || panic!("asleep observation accepted")));
        controller.set_sleeping(false);
        assert!(!controller.request_recovery("manualRetry"));
        let (latest, _, reason) = controller.recovery_request();
        assert_eq!(reason, "manualRetry");
        assert!(controller.finish_recovery(latest));
        assert!(controller.request_recovery("connectionLost"));
    }

    #[test]
    fn closed_pane_rejects_late_installs_and_recovery_requests() {
        let controller = RemoteAttachmentController::new();
        let (generation, _) = controller.begin_generation();
        controller.request_recovery("systemWake");
        let (revision, _, _) = controller.recovery_request();
        controller.cancel_recovery();
        assert!(!controller.recovery_is_current(revision));
        assert!(!controller.request_recovery("connectionLost"));
        assert!(
            controller
                .install_if_current(generation, attachment(Default::default()))
                .is_err()
        );
    }

    #[test]
    fn stable_writer_follows_the_current_generation_and_fails_while_offline() {
        let controller = RemoteAttachmentController::new();
        let writer = controller.stable_writer();
        let first_output = Arc::new(Mutex::new(Vec::new()));
        let second_output = Arc::new(Mutex::new(Vec::new()));

        let (first, previous) = controller.begin_generation();
        assert!(previous.is_none());
        assert!(
            controller
                .install_if_current(first, attachment(first_output.clone()))
                .is_ok()
        );
        writer.lock().unwrap().write_all(b"first").unwrap();

        let (second, previous) = controller.begin_generation();
        assert!(previous.is_some());
        let error = writer.lock().unwrap().write_all(b"offline").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert!(
            controller
                .install_if_current(second, attachment(second_output.clone()))
                .is_ok()
        );
        writer.lock().unwrap().write_all(b"second").unwrap();

        assert_eq!(&*first_output.lock().unwrap(), b"first");
        assert_eq!(&*second_output.lock().unwrap(), b"second");
    }

    #[test]
    fn stale_install_and_eof_cannot_replace_or_clear_a_newer_attachment() {
        let controller = RemoteAttachmentController::new();
        let (first, _) = controller.begin_generation();
        let (second, _) = controller.begin_generation();
        assert!(
            controller
                .install_if_current(second, attachment(Default::default()))
                .is_ok()
        );

        assert!(
            controller
                .install_if_current(first, attachment(Default::default()))
                .is_err()
        );
        assert!(controller.clear_if_current(first).is_none());
        assert!(controller.is_current(second));
        assert!(controller.current_master().is_some());
        assert!(controller.current_child().is_some());

        assert!(controller.clear_if_current(second).is_some());
        assert!(!controller.is_current(second));
        assert!(controller.current_master().is_none());
    }

    #[test]
    fn remote_backend_owns_one_stable_writer_and_backlog() {
        let controller = RemoteAttachmentController::new();
        let backlog = Arc::new(Mutex::new(Default::default()));
        let commands = crate::host::RemoteTmuxCommands {
            version_argv: Vec::new(),
            create_argv: Vec::new(),
            configure_argv: Vec::new(),
            attach_argv: Vec::new(),
            probe_argv: Vec::new(),
            clients_argv: Vec::new(),
            capture_argv: Vec::new(),
            capture_full_argv: Vec::new(),
            activity_argv: Vec::new(),
            kill_argv: Vec::new(),
            forward_cleanup_argv: Vec::new(),
            support_cleanup_argv: Vec::new(),
            remote_socket_path: "/tmp/qmux-test.sock".to_string(),
        };
        let history = RemoteHistoryCheckpoint::new(Vec::new());
        let backend = crate::state::RemoteTmuxBackend::new(
            controller,
            history,
            backlog.clone(),
            commands,
            true,
        );

        assert!(Arc::ptr_eq(&backend.backlog, &backlog));
        assert!(backend.native_surface);
        assert!(matches!(
            crate::state::PaneBackend::RemoteTmux(backend),
            crate::state::PaneBackend::RemoteTmux(_)
        ));
    }

    #[test]
    fn history_checkpoint_emits_only_new_bytes_and_survives_rollover() {
        let checkpoint = RemoteHistoryCheckpoint::new(b"one\ntwo\n".to_vec());
        assert_eq!(checkpoint.delta(b"one\ntwo\nthree\n"), b"three\n");
        checkpoint.advance(b"one\ntwo\nthree\n");
        assert!(checkpoint.delta(b"one\ntwo\nthree\n").is_empty());

        let old = [vec![b'x'; 2_000], b"\nanchor\n".to_vec()].concat();
        let checkpoint = RemoteHistoryCheckpoint::new(old);
        assert_eq!(
            checkpoint.delta(b"rolled off\nanchor\nafter rollover\n"),
            b"after rollover\n"
        );
    }

    #[test]
    fn history_checkpoint_replays_everything_without_a_safe_overlap() {
        let checkpoint = RemoteHistoryCheckpoint::new(b"old host history\n".to_vec());
        assert_eq!(
            checkpoint.delta(b"entirely replaced history\n"),
            b"entirely replaced history\n"
        );
    }

    #[test]
    fn repeated_short_overlap_replays_instead_of_dropping_between_matches() {
        let checkpoint = RemoteHistoryCheckpoint::new(b"prompt> ".to_vec());
        let capture = b"prompt> output that must stay\nprompt> ";
        assert_eq!(checkpoint.delta(capture), capture);
    }

    #[test]
    fn reconnect_backoff_survives_generations_until_verified() {
        let controller = RemoteAttachmentController::new();
        assert_eq!(
            controller.next_reconnect_delay(Duration::from_millis(250), Duration::from_secs(10)),
            Duration::from_millis(250)
        );
        controller.begin_generation();
        assert_eq!(
            controller.next_reconnect_delay(Duration::from_millis(250), Duration::from_secs(10)),
            Duration::from_millis(500)
        );
        assert!(!controller.mark_attachment_live_if_current(1));
        assert!(
            controller
                .install_if_current(1, attachment(Default::default()))
                .is_ok()
        );
        assert!(controller.mark_attachment_live_if_current(1));
        assert_eq!(
            controller.next_reconnect_delay(Duration::from_millis(250), Duration::from_secs(10)),
            Duration::from_millis(250)
        );
        controller.begin_generation();
        assert!(!controller.mark_attachment_live_if_current(1));
    }
}
