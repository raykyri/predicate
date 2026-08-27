//! Journal storage and tweet hydration. The journal's semantic format —
//! what an entry is, what a hydrated tweet snapshot looks like — is defined
//! once, in the frontend (src/lib/journal.ts). The backend deliberately
//! treats entries as opaque records: it persists them inside state.json,
//! dedupes by id, and drops anything that is not an object with a string
//! id, mirroring how the frontend's normalizeJournalState drops malformed
//! entries instead of the whole journal. Keeping the schema in one place is
//! what lets the entry format grow (grouping, attached questions) without a
//! Rust/TypeScript definition drifting apart.
//!
//! Tweet hydration goes through here because the webview cannot reach X:
//! the CSP has no connect-src for it and the syndication CDN's CORS only
//! admits platform.twitter.com. The command is a narrow proxy — it accepts
//! a numeric status id plus the widget-derived token, always constructs the
//! cdn.syndication.twimg.com URL itself, and returns the raw JSON body for
//! the frontend to normalize.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JOURNAL_STATE_VERSION: u32 = 1;

fn journal_state_version() -> u32 {
    JOURNAL_STATE_VERSION
}

/// Versioned envelope over the journal's entry list. Entries are opaque
/// JSON records (see module docs); order is append order, oldest first.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalState {
    #[serde(default = "journal_state_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Value>,
}

impl Default for JournalState {
    fn default() -> Self {
        Self {
            version: JOURNAL_STATE_VERSION,
            entries: Vec::new(),
        }
    }
}

impl JournalState {
    /// serde skip guard: an untouched journal serializes to nothing, so
    /// state files from builds that predate it round-trip byte-identically.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Structural normalization only: keep objects that carry a non-empty
/// string id, first occurrence of each id wins. Semantic validation is the
/// frontend's (see module docs).
pub fn normalize_journal_state(state: &mut JournalState) {
    let mut seen = std::collections::HashSet::new();
    state.entries.retain(|entry| {
        entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .is_some_and(|id| seen.insert(id.to_string()))
    });
    state.version = JOURNAL_STATE_VERSION;
}

fn http_client() -> Result<reqwest::Client, String> {
    crate::ensure_rustls_crypto_provider()?;
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to build tweet HTTP client: {error}"))
}

fn validate_tweet_fetch_args(id: &str, token: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 25 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid tweet id".to_string());
    }
    if token.is_empty() || token.len() > 32 || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("invalid tweet token".to_string());
    }
    Ok(())
}

/// Fetch a tweet's syndication payload by status id. `token` is the derived
/// query parameter the endpoint expects; both inputs are validated to shape
/// only — the URL is always built here, never taken from the caller.
pub async fn fetch_tweet_json(id: &str, token: &str) -> Result<String, String> {
    validate_tweet_fetch_args(id, token)?;
    let url =
        format!("https://cdn.syndication.twimg.com/tweet-result?id={id}&token={token}&lang=en");
    let response = http_client()?
        .get(url)
        .header("User-Agent", "qmux")
        .send()
        .await
        .map_err(|error| format!("tweet fetch failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read tweet response: {error}"))?;
    if !status.is_success() {
        return Err(format!("tweet fetch failed: HTTP {status}"));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_drops_idless_and_duplicate_entries() {
        let mut state = JournalState {
            version: 7,
            entries: vec![
                json!({"id": "a", "kind": "note", "text": "first"}),
                json!({"kind": "note", "text": "no id"}),
                json!({"id": "", "kind": "note"}),
                json!("not an object"),
                json!({"id": "a", "kind": "note", "text": "duplicate"}),
                json!({"id": "b", "kind": "tweet", "url": "https://x.com/x/status/1"}),
            ],
        };
        normalize_journal_state(&mut state);
        assert_eq!(state.version, JOURNAL_STATE_VERSION);
        let ids: Vec<_> = state
            .entries
            .iter()
            .map(|entry| entry.get("id").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert_eq!(
            state.entries[0].get("text").and_then(Value::as_str),
            Some("first"),
        );
    }

    #[test]
    fn journal_state_round_trips_through_json() {
        let state = JournalState {
            version: JOURNAL_STATE_VERSION,
            entries: vec![json!({"id": "a", "kind": "note", "text": "hi"})],
        };
        let text = serde_json::to_string(&state).unwrap();
        let back: JournalState = serde_json::from_str(&text).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn empty_journal_state_is_empty() {
        assert!(JournalState::default().is_empty());
    }

    #[test]
    fn fetch_rejects_malformed_inputs() {
        for (id, token) in [
            ("", "abc"),
            ("12x", "abc"),
            ("12345678901234567890123456", "abc"),
            ("20", ""),
            ("20", "bad token"),
            ("20", "../etc"),
        ] {
            assert!(validate_tweet_fetch_args(id, token).is_err(), "{id} {token}");
        }
        assert!(validate_tweet_fetch_args("20", "6dq1a2xwd93").is_ok());
    }
}
