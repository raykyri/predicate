use crate::events::QmuxEvent;
use crate::state::{AppState, ShellAgentJobInfo, ShellAgentJobState};
use serde_json::json;
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

const SHELL_JOB_POLL_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessJobSample {
    process_group_id: i32,
    terminal_foreground_group_id: i32,
    stopped: bool,
    zombie: bool,
}

impl ProcessJobSample {
    fn job_state(self) -> ShellAgentJobState {
        if self.stopped {
            ShellAgentJobState::Stopped
        } else if self.process_group_id > 0
            && self.process_group_id == self.terminal_foreground_group_id
        {
            ShellAgentJobState::Foreground
        } else {
            ShellAgentJobState::Backgrounded
        }
    }
}

pub fn start_shell_job_monitor(state: AppState) {
    std::thread::spawn(move || {
        loop {
            let targets = state.shell_agent_job_targets();
            if !targets.is_empty() {
                let pids = targets
                    .iter()
                    .map(|target| target.supervisor_pid)
                    .collect::<Vec<_>>();
                if let Some(samples) = process_job_samples(&pids) {
                    for target in targets {
                        match samples
                            .get(&target.supervisor_pid)
                            .filter(|sample| !sample.zombie)
                        {
                            Some(sample) => {
                                if let Some(info) = state.update_shell_agent_job_sample(
                                    &target.job_id,
                                    sample.job_state(),
                                ) {
                                    recover_shell_terminal_modes(&state, &info);
                                    emit_job_state(&state, &info);
                                }
                            }
                            None => {
                                if let Some(info) =
                                    state.note_shell_agent_job_missing(&target.job_id)
                                {
                                    retire_missing_job(&state, info);
                                }
                            }
                        }
                    }
                }
            }
            std::thread::sleep(SHELL_JOB_POLL_INTERVAL);
        }
    });
}

fn recover_shell_terminal_modes(state: &AppState, info: &ShellAgentJobInfo) {
    if !job_state_returns_terminal_to_shell(info.state) {
        return;
    }
    if let Err(err) = crate::pty::reset_live_pane_terminal_modes(state, &info.pane_id) {
        eprintln!(
            "qmux: failed to reset live terminal modes for shell job {}: {err}",
            info.job_id
        );
    }
}

fn job_state_returns_terminal_to_shell(state: ShellAgentJobState) -> bool {
    matches!(
        state,
        ShellAgentJobState::Stopped | ShellAgentJobState::Backgrounded
    )
}

pub fn emit_job_state(state: &AppState, info: &ShellAgentJobInfo) {
    state.emit(QmuxEvent::new(
        "agent.shell_job_state_changed",
        Some(info.pane_id.clone()),
        Some(info.agent_id.clone()),
        json!({ "job": info }),
    ));
}

pub fn emit_job_removed(state: &AppState, info: &ShellAgentJobInfo) {
    state.emit(QmuxEvent::new(
        "agent.shell_job_removed",
        Some(info.pane_id.clone()),
        Some(info.agent_id.clone()),
        json!({ "jobId": info.job_id }),
    ));
}

fn retire_missing_job(state: &AppState, info: ShellAgentJobInfo) {
    emit_job_removed(state, &info);
    match crate::workspace::detach_pane_agent_if_matches(state, &info.pane_id, &info.agent_id) {
        Ok(Some(_)) => {
            if let Err(err) = crate::pty::reset_pane_terminal_modes(state, &info.pane_id) {
                eprintln!(
                    "qmux: failed to reset terminal modes for vanished shell job {}: {err}",
                    info.job_id
                );
            }
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!(
                "qmux: failed to detach vanished shell agent job {}: {err}",
                info.job_id
            );
        }
    }
}

fn process_job_samples(pids: &[u32]) -> Option<HashMap<u32, ProcessJobSample>> {
    if pids.is_empty() {
        return Some(HashMap::new());
    }
    let pid_list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new("/bin/ps")
        .arg("-p")
        .arg(pid_list)
        .arg("-o")
        .arg("pid=")
        .arg("-o")
        .arg("pgid=")
        .arg("-o")
        .arg("tpgid=")
        .arg("-o")
        .arg("stat=")
        .output()
        .ok()?;
    // `ps` may return nonzero when requested pids vanished, including when every
    // pid is gone and stdout is empty. Only treat an empty failed invocation as
    // non-authoritative when `ps` also reported an actual diagnostic.
    if !output.status.success() && output.stdout.is_empty() && !output.stderr.is_empty() {
        return None;
    }
    Some(parse_process_job_samples(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_process_job_samples(output: &str) -> HashMap<u32, ProcessJobSample> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let process_group_id = parts.next()?.parse::<i32>().ok()?;
            let terminal_foreground_group_id = parts.next()?.parse::<i32>().ok()?;
            let status = parts.next()?;
            Some((
                pid,
                ProcessJobSample {
                    process_group_id,
                    terminal_foreground_group_id,
                    stopped: status.contains('T'),
                    zombie: status.contains('Z'),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_foreground_background_and_stopped_jobs() {
        let samples = parse_process_job_samples(
            "  10  20  20 S+\n  11  21  20 S\n  12  22  20 T\n  13  23  20 Z\n",
        );

        assert_eq!(
            samples.get(&10).unwrap().job_state(),
            ShellAgentJobState::Foreground
        );
        assert_eq!(
            samples.get(&11).unwrap().job_state(),
            ShellAgentJobState::Backgrounded
        );
        assert_eq!(
            samples.get(&12).unwrap().job_state(),
            ShellAgentJobState::Stopped
        );
        assert!(samples.get(&13).unwrap().zombie);
    }

    #[test]
    fn malformed_process_rows_are_ignored() {
        let samples = parse_process_job_samples("bad row\n  10  nope 20 S\n  11 21 20 S\n");
        assert_eq!(samples.len(), 1);
        assert!(samples.contains_key(&11));
    }

    #[test]
    fn only_shell_owned_job_states_request_live_mode_recovery() {
        assert!(!job_state_returns_terminal_to_shell(
            ShellAgentJobState::Foreground
        ));
        assert!(job_state_returns_terminal_to_shell(
            ShellAgentJobState::Stopped
        ));
        assert!(job_state_returns_terminal_to_shell(
            ShellAgentJobState::Backgrounded
        ));
    }
}
