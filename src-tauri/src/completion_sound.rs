use crate::events::QmuxEvent;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const DEFAULT_SOUND_ID: &str = "chime";
const COMPLETION_COALESCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CompletionSoundOption {
    id: String,
    label: String,
    system_name: Option<String>,
}

static SOUND_OPTIONS: LazyLock<Vec<CompletionSoundOption>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../src/assets/completion-sounds.json"))
        .expect("completion-sounds.json must contain a valid sound catalog")
});

pub fn system_name_for_id(sound_id: &str) -> Result<Option<&'static str>, String> {
    SOUND_OPTIONS
        .iter()
        .find(|option| option.id == sound_id)
        .map(|option| option.system_name.as_deref())
        .ok_or_else(|| format!("unknown completion sound id {sound_id:?}"))
}

pub struct CompletionSoundState {
    selected_id: String,
    working_agent_ids: HashSet<String>,
    research_agent_ids: HashSet<String>,
    research_group_ids: HashSet<String>,
    last_completion_at: Option<Instant>,
}

impl Default for CompletionSoundState {
    fn default() -> Self {
        Self {
            selected_id: DEFAULT_SOUND_ID.to_string(),
            working_agent_ids: HashSet::new(),
            research_agent_ids: HashSet::new(),
            research_group_ids: HashSet::new(),
            last_completion_at: None,
        }
    }
}

impl CompletionSoundState {
    pub fn set_selected_id(&mut self, sound_id: &str) -> Result<(), String> {
        system_name_for_id(sound_id)?;
        self.selected_id = sound_id.to_string();
        Ok(())
    }

    pub fn mark_research_agent(&mut self, agent_id: &str) {
        self.research_agent_ids.insert(agent_id.to_string());
    }

    pub fn mark_research_group(&mut self, group_id: &str) {
        self.research_group_ids.insert(group_id.to_string());
    }

    /// Returns the allowlisted macOS system sound name for a genuine live chat
    /// completion. Lifecycle state lives in the app process, so a WebView crash
    /// or reload cannot erase a Running observation before the matching Done.
    pub fn observe_event(&mut self, event: &QmuxEvent) -> Option<String> {
        self.observe_event_at(event, Instant::now())
    }

    fn observe_event_at(&mut self, event: &QmuxEvent, now: Instant) -> Option<String> {
        if event.event_type == "group.created"
            && let Some(group) = event.payload.get("group")
            && group.get("scope").and_then(|value| value.as_str()) == Some("research")
            && let Some(group_id) = group.get("id").and_then(|value| value.as_str())
        {
            self.mark_research_group(group_id);
        } else if event.event_type == "group.removed"
            && let Some(group_id) = event
                .payload
                .get("groupId")
                .and_then(|value| value.as_str())
        {
            self.research_group_ids.remove(group_id);
        }

        let agent_id = event.agent_id.as_deref()?;
        if event.event_type == "agent.spawned"
            && event.payload.get("source").and_then(|value| value.as_str()) == Some("research")
        {
            self.mark_research_agent(agent_id);
        }

        let status = event
            .payload
            .get("agent")
            .and_then(|agent| agent.get("status"))
            .and_then(|status| status.as_str())?;
        if event
            .payload
            .get("agent")
            .and_then(|agent| agent.get("groupId"))
            .and_then(|group_id| group_id.as_str())
            .is_some_and(|group_id| self.research_group_ids.contains(group_id))
        {
            self.mark_research_agent(agent_id);
        }
        let was_working = self.working_agent_ids.contains(agent_id);
        let is_working = matches!(status, "starting" | "running");
        let should_track = is_working && event.event_type != "agent.recovered";

        if should_track {
            self.working_agent_ids.insert(agent_id.to_string());
        } else {
            self.working_agent_ids.remove(agent_id);
        }

        if event.event_type != "agent.done"
            || status != "done"
            || !was_working
            || self.research_agent_ids.contains(agent_id)
        {
            return None;
        }

        if self
            .last_completion_at
            .is_some_and(|previous| now.duration_since(previous) < COMPLETION_COALESCE)
        {
            return None;
        }
        self.last_completion_at = Some(now);
        system_name_for_id(&self.selected_id)
            .ok()
            .flatten()
            .map(ToString::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent_event(event_type: &str, agent_id: &str, status: &str) -> QmuxEvent {
        QmuxEvent::new(
            event_type,
            None,
            Some(agent_id.to_string()),
            json!({ "agent": { "status": status } }),
        )
    }

    fn grouped_agent_event(
        event_type: &str,
        agent_id: &str,
        group_id: &str,
        status: &str,
    ) -> QmuxEvent {
        QmuxEvent::new(
            event_type,
            None,
            Some(agent_id.to_string()),
            json!({ "agent": { "groupId": group_id, "status": status } }),
        )
    }

    #[test]
    fn catalog_is_the_expected_allowlist() {
        assert_eq!(system_name_for_id("chime"), Ok(Some("Glass")));
        assert_eq!(system_name_for_id("none"), Ok(None));
        assert!(system_name_for_id("../../arbitrary").is_err());
    }

    #[test]
    fn lifecycle_survives_frontend_absence_and_sounds_only_on_final_done() {
        let mut state = CompletionSoundState::default();
        let start = Instant::now();
        assert_eq!(
            state.observe_event_at(&agent_event("agent.running", "agent-1", "running"), start),
            None
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.running", "agent-1", "running"),
                start + Duration::from_millis(500),
            ),
            None
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "agent-1", "done"),
                start + Duration::from_secs(1),
            ),
            Some("Glass".to_string())
        );
    }

    #[test]
    fn recovery_research_interruptions_and_bursts_are_suppressed() {
        let start = Instant::now();
        let mut state = CompletionSoundState::default();
        state.observe_event_at(
            &agent_event("agent.recovered", "recovered", "running"),
            start,
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "recovered", "done"),
                start + Duration::from_secs(1),
            ),
            None
        );

        state.mark_research_agent("research");
        state.observe_event_at(&agent_event("agent.running", "research", "running"), start);
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "research", "done"),
                start + Duration::from_secs(1),
            ),
            None
        );

        // Group ownership is known before a research process can race its
        // binding, so an instantly-completing run is still suppressed.
        state.mark_research_group("research-group");
        state.observe_event_at(
            &grouped_agent_event(
                "agent.running",
                "fast-research",
                "research-group",
                "running",
            ),
            start,
        );
        assert_eq!(
            state.observe_event_at(
                &grouped_agent_event("agent.done", "fast-research", "research-group", "done",),
                start + Duration::from_secs(1),
            ),
            None
        );

        state.observe_event_at(
            &agent_event("agent.running", "interrupted", "running"),
            start,
        );
        state.observe_event_at(
            &agent_event("agent.awaiting_input", "interrupted", "awaitingInput"),
            start + Duration::from_millis(500),
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "interrupted", "done"),
                start + Duration::from_secs(1),
            ),
            None
        );

        for id in ["one", "two"] {
            state.observe_event_at(&agent_event("agent.running", id, "running"), start);
        }
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "one", "done"),
                start + Duration::from_secs(2),
            ),
            Some("Glass".to_string())
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "two", "done"),
                start + Duration::from_millis(2_100),
            ),
            None
        );
    }
}
