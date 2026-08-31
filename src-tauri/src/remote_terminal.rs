use crate::state::{SharedChild, SharedMaster, SharedWriter};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
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
}

impl RemoteAttachmentController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
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
        let previous = state.current.take();
        (state.generation, previous)
    }

    /// Carries reconnect backoff across short-lived SSH generations. A process
    /// that spawns successfully and exits before delivering terminal bytes is
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
    /// superseded reader can race its final bytes with the next
    /// `begin_generation`; those bytes must not reset backoff or report the new
    /// generation as connected.
    pub fn mark_attachment_live_if_current(&self, generation: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.generation != generation {
            return false;
        }
        self.reconnect_failures.store(0, Ordering::Relaxed);
        true
    }

    /// Installs an attachment only if no newer attempt has begun.
    pub fn install_if_current(
        &self,
        generation: u64,
        attachment: RemoteAttachment,
    ) -> Result<(), RemoteAttachment> {
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
    fn reconnect_backoff_survives_attachment_generations_until_bytes_arrive() {
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
        assert!(controller.mark_attachment_live_if_current(1));
        assert_eq!(
            controller.next_reconnect_delay(Duration::from_millis(250), Duration::from_secs(10)),
            Duration::from_millis(250)
        );
        controller.begin_generation();
        assert!(!controller.mark_attachment_live_if_current(1));
    }
}
