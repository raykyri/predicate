use crate::events::QmuxEvent;
use serde::Deserialize;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

const DEFAULT_SOUND_ID: &str = "default";
const COMPLETION_COALESCE: Duration = Duration::from_millis(250);

const SUCCESS_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/success.wav");
const CONFIRMATION_SOUND_BYTES: &[u8] =
    include_bytes!("../../src/assets/sounds/completion/confirmation.mp3");
const CHIME_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/chime.wav");
const LIGHT_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/light.wav");
const WATER_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/water.wav");
const WARP_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/warp.mp3");
const SWITCH_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/switch.mp3");
const DIGITAL_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/digital.mp3");
const POWER_UP_SOUND_BYTES: &[u8] =
    include_bytes!("../../src/assets/sounds/completion/power-up.mp3");
const EVENT_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/event.mp3");
const DRUM_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/drum.mp3");
const QUEST_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/quest.mp3");
const IMPACT_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/impact.mp3");
const POTS_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/pots.mp3");
const BELL_SOUND_BYTES: &[u8] = include_bytes!("../../src/assets/sounds/completion/bell.mp3");

fn bundled_sound(name: &str) -> Option<CompletionSound> {
    let (name, bytes) = match name {
        "success" => ("success", SUCCESS_SOUND_BYTES),
        "confirmation" => ("confirmation", CONFIRMATION_SOUND_BYTES),
        "chime" => ("chime", CHIME_SOUND_BYTES),
        "light" => ("light", LIGHT_SOUND_BYTES),
        "water" => ("water", WATER_SOUND_BYTES),
        "warp" => ("warp", WARP_SOUND_BYTES),
        "switch" => ("switch", SWITCH_SOUND_BYTES),
        "digital" => ("digital", DIGITAL_SOUND_BYTES),
        "power-up" => ("power-up", POWER_UP_SOUND_BYTES),
        "event" => ("event", EVENT_SOUND_BYTES),
        "drum" => ("drum", DRUM_SOUND_BYTES),
        "quest" => ("quest", QUEST_SOUND_BYTES),
        "impact" => ("impact", IMPACT_SOUND_BYTES),
        "pots" => ("pots", POTS_SOUND_BYTES),
        "bell" => ("bell", BELL_SOUND_BYTES),
        _ => return None,
    };
    Some(CompletionSound::Bundled { name, bytes })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CompletionSoundOption {
    id: String,
    label: String,
    bundled_name: Option<String>,
    system_name: Option<String>,
}

static SOUND_OPTIONS: LazyLock<Vec<CompletionSoundOption>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../src/assets/completion-sounds.json"))
        .expect("completion-sounds.json must contain a valid sound catalog")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionSound {
    System(&'static str),
    Bundled {
        name: &'static str,
        bytes: &'static [u8],
    },
}

pub fn sound_for_id(sound_id: &str) -> Result<Option<CompletionSound>, String> {
    let option = SOUND_OPTIONS
        .iter()
        .find(|option| option.id == sound_id)
        .ok_or_else(|| format!("unknown completion sound id {sound_id:?}"))?;

    match (
        option.bundled_name.as_deref(),
        option.system_name.as_deref(),
    ) {
        (Some(name), None) => bundled_sound(name)
            .map(Some)
            .ok_or_else(|| format!("unknown bundled completion sound {name:?}")),
        (None, Some(name)) => Ok(Some(CompletionSound::System(name))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!(
            "completion sound {sound_id:?} cannot be both bundled and system-provided"
        )),
    }
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
        sound_for_id(sound_id)?;
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

        // Workspace refreshes carry a full AgentInfo for the frontend's
        // surgical display update, but they do not prove that an agent started,
        // resumed, or finished work. Letting their persisted status participate
        // here could arm a restored `running` agent for a false completion sound
        // merely because a sibling shell observed a branch change.
        if event.event_type == "agent.workspace_changed" {
            return None;
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
        sound_for_id(&self.selected_id)
            .ok()
            .flatten()
            .map(|_| self.selected_id.clone())
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
        assert_eq!(sound_for_id("none"), Ok(None));
        assert!(matches!(
            sound_for_id("default"),
            Ok(Some(CompletionSound::Bundled {
                name: "success",
                ..
            }))
        ));
        assert!(matches!(
            sound_for_id("confirmation"),
            Ok(Some(CompletionSound::Bundled {
                name: "confirmation",
                ..
            }))
        ));
        assert!(matches!(
            sound_for_id("chime"),
            Ok(Some(CompletionSound::Bundled { name: "chime", .. }))
        ));
        assert!(matches!(
            sound_for_id("light"),
            Ok(Some(CompletionSound::Bundled { name: "light", .. }))
        ));
        assert!(matches!(
            sound_for_id("water"),
            Ok(Some(CompletionSound::Bundled { name: "water", .. }))
        ));
        assert!(matches!(
            sound_for_id("warp"),
            Ok(Some(CompletionSound::Bundled { name: "warp", .. }))
        ));
        assert!(matches!(
            sound_for_id("switch"),
            Ok(Some(CompletionSound::Bundled { name: "switch", .. }))
        ));
        assert!(matches!(
            sound_for_id("digital"),
            Ok(Some(CompletionSound::Bundled {
                name: "digital",
                ..
            }))
        ));
        assert!(matches!(
            sound_for_id("power-up"),
            Ok(Some(CompletionSound::Bundled {
                name: "power-up",
                ..
            }))
        ));
        assert!(matches!(
            sound_for_id("event"),
            Ok(Some(CompletionSound::Bundled { name: "event", .. }))
        ));
        assert!(matches!(
            sound_for_id("drum"),
            Ok(Some(CompletionSound::Bundled { name: "drum", .. }))
        ));
        assert!(matches!(
            sound_for_id("quest"),
            Ok(Some(CompletionSound::Bundled { name: "quest", .. }))
        ));
        assert!(matches!(
            sound_for_id("impact"),
            Ok(Some(CompletionSound::Bundled { name: "impact", .. }))
        ));
        assert!(matches!(
            sound_for_id("pots"),
            Ok(Some(CompletionSound::Bundled { name: "pots", .. }))
        ));
        assert!(matches!(
            sound_for_id("bell"),
            Ok(Some(CompletionSound::Bundled { name: "bell", .. }))
        ));
        for removed in ["pop", "tink", "purr", "ping", "blip"] {
            assert!(sound_for_id(removed).is_err());
        }
        assert!(sound_for_id("../../arbitrary").is_err());
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
            Some("default".to_string())
        );
    }

    #[test]
    fn workspace_changes_do_not_arm_or_disarm_completion_tracking() {
        let start = Instant::now();
        let mut state = CompletionSoundState::default();

        // A persisted running status carried only to refresh branch display is
        // not evidence that this process observed the agent begin working.
        state.observe_event_at(
            &agent_event("agent.workspace_changed", "restored", "running"),
            start,
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "restored", "done"),
                start + Duration::from_secs(1),
            ),
            None
        );

        // Conversely, a workspace payload with a stale resting status must not
        // erase a real running observation before its matching completion.
        state.observe_event_at(
            &agent_event("agent.running", "live", "running"),
            start + Duration::from_secs(2),
        );
        state.observe_event_at(
            &agent_event("agent.workspace_changed", "live", "done"),
            start + Duration::from_millis(2_500),
        );
        assert_eq!(
            state.observe_event_at(
                &agent_event("agent.done", "live", "done"),
                start + Duration::from_secs(3),
            ),
            Some("default".to_string())
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
            Some("default".to_string())
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
