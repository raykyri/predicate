use crate::{persistence, state::AppState};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const GITHUB_API_VERSION: &str = "2026-03-10";
const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USER_AGENT: &str = "qmux-publisher";
const KEYCHAIN_SERVICE: &str = "app.qmux.github-oauth";
const KEYCHAIN_ACCOUNT: &str = "github";
const PUBLICATIONS_FILE: &str = "publications.json";
const PUBLICATION_INDEX_FILE: &str = "publication.json";
const PUBLICATION_README_FILE: &str = "README.md";
const MAX_PUBLICATION_FILES: usize = 250;
const MAX_PUBLICATION_FILE_BYTES: usize = 10_000_000;
const MAX_PUBLICATION_TOTAL_BYTES: usize = 12_000_000;
const MAX_GIST_COMMENTS: usize = 300;
/// Comments fetched per page. Sized together with the response-byte cap below
/// so a page of maximum-size comments always fits: gist comment bodies run up
/// to 65,536 characters, or ~400 KB after worst-case JSON escaping, and any
/// GitHub user can post them on a visible gist. At 100 per page a spammer's
/// page overflowed the old 5 MB cap and the fail-closed read error disabled
/// proposal listing and resolution for the publication until the comments were
/// hand-deleted on GitHub.
const GIST_COMMENTS_PER_PAGE: usize = 25;
const MAX_GITHUB_COMMENTS_RESPONSE_BYTES: usize = 16_000_000;
const MAX_PROPOSAL_PROMPT_CHARACTERS: usize = 10_000;
const MAX_PROPOSAL_ANSWER_CHARACTERS: usize = 40_000;
const MAX_PROPOSAL_QUOTE_CHARACTERS: usize = 2_000;
const MAX_PROPOSAL_CONTEXT_CHARACTERS: usize = 500;
const PROPOSAL_MARKER_PREFIX: &str = "<!-- qmux-proposal:v1 ";
const PROPOSAL_RESOLUTION_MARKER_PREFIX: &str = "<!-- qmux-proposal-resolution:v1 ";
const COMMENT_MARKER_SUFFIX: &str = " -->";

static PUBLICATIONS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishingAuthStatus {
    configured: bool,
    connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishingDeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_at: u64,
    interval_seconds: u64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum PublishingAuthPollResult {
    Pending {
        #[serde(rename = "intervalSeconds")]
        interval_seconds: u64,
    },
    Connected {
        account: PublishingAuthStatus,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationSource {
    kind: String,
    #[serde(flatten)]
    detail: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishPublicationRequest {
    publication_id: String,
    title: String,
    is_public: bool,
    files: BTreeMap<String, String>,
    source: PublicationSource,
    #[serde(default)]
    public_node_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPublicationRequest {
    publication_id: String,
    title: String,
    is_public: bool,
    files: BTreeMap<String, String>,
    source: PublicationSource,
    #[serde(default)]
    public_node_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationBinding {
    publication_id: String,
    gist_id: String,
    gist_url: String,
    share_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    is_public: bool,
    source: PublicationSource,
    #[serde(default)]
    public_node_ids: BTreeMap<String, String>,
    #[serde(default)]
    proposal_states: BTreeMap<String, PublicationProposalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationProposalState {
    proposal_comment_id: u64,
    status: String,
    author_login: String,
    parent_public_node_id: String,
    prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer_markdown: Option<String>,
    /// The proposal's quoted-passage anchor, kept verbatim: the resolution
    /// digest recomputed at republish time must match the original comment's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<ResearchProposalAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution_comment_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published_public_node_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationProposal {
    comment_id: u64,
    author_login: String,
    author_url: String,
    parent_public_node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_node_id: Option<String>,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<ResearchProposalAnchor>,
    created_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_node_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvePublicationProposalRequest {
    publication_id: String,
    proposal_comment_id: u64,
    status: String,
    #[serde(default)]
    local_node_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicationStore {
    #[serde(default = "publication_store_version")]
    version: u32,
    #[serde(default)]
    bindings: Vec<PublicationBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredCredential {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GitHubGist {
    id: String,
    html_url: String,
    #[serde(default)]
    description: Option<String>,
    public: bool,
    #[serde(default)]
    files: BTreeMap<String, GitHubGistFile>,
    #[serde(default)]
    owner: Option<GitHubUser>,
    #[serde(default)]
    history: Vec<GitHubGistRevision>,
}

#[derive(Debug, Deserialize)]
struct GitHubGistRevision {
    version: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubGistComment {
    id: u64,
    body: String,
    created_at: String,
    #[serde(default)]
    user: Option<GitHubCommentUser>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubCommentUser {
    login: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResearchProposalPayload {
    publication_id: String,
    parent_node_id: String,
    prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer_markdown: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<ResearchProposalAnchor>,
}

/// The quoted passage an anchored proposal was asked about, mirroring the
/// web/TS `ResearchProposalAnchor` shape (and its digest contribution).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResearchProposalAnchor {
    start: u64,
    end: u64,
    exact: String,
    prefix: String,
    suffix: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposalResolutionPayload {
    publication_id: String,
    proposal_comment_id: u64,
    proposal_digest: String,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_node_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubGistFile {
    #[serde(default)]
    size: usize,
    #[serde(default)]
    truncated: bool,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    raw_url: Option<String>,
}

struct RemoteGist {
    gist: GitHubGist,
    etag: Option<String>,
}

#[derive(Serialize)]
struct CreateGistRequest<'a> {
    description: String,
    public: bool,
    files: BTreeMap<&'a str, CreateGistFile<'a>>,
}

#[derive(Serialize)]
struct CreateGistFile<'a> {
    content: &'a str,
}

#[derive(Serialize)]
struct UpdateGistRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    files: BTreeMap<String, Option<UpdateGistFile>>,
}

#[derive(Serialize)]
struct UpdateGistFile {
    content: String,
}

#[tauri::command]
pub async fn publishing_auth_status() -> Result<PublishingAuthStatus, String> {
    auth_status()
}

#[tauri::command]
pub async fn publishing_auth_begin() -> Result<PublishingDeviceAuthorization, String> {
    let client_id = github_client_id().ok_or_else(|| {
        "GitHub publishing is not configured. Set QMUX_GITHUB_CLIENT_ID when building qmux."
            .to_string()
    })?;
    let client = http_client()?;
    let response = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", client_id.as_str()), ("scope", "gist")])
        .send()
        .await
        .map_err(|error| github_request_error("start GitHub authorization", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read GitHub authorization response: {error}"))?;
    if !status.is_success() {
        return Err(github_response_error(
            "start GitHub authorization",
            status,
            &body,
        ));
    }
    let authorization: DeviceCodeResponse = serde_json::from_str(&body)
        .map_err(|error| format!("invalid GitHub authorization response: {error}"))?;
    Ok(PublishingDeviceAuthorization {
        device_code: authorization.device_code,
        user_code: authorization.user_code,
        verification_uri: authorization.verification_uri,
        expires_at: now_millis().saturating_add(authorization.expires_in.saturating_mul(1_000)),
        interval_seconds: authorization.interval.max(1),
    })
}

#[tauri::command]
pub async fn publishing_auth_poll(device_code: String) -> Result<PublishingAuthPollResult, String> {
    let client_id = github_client_id().ok_or_else(|| {
        "GitHub publishing is not configured. Set QMUX_GITHUB_CLIENT_ID when building qmux."
            .to_string()
    })?;
    if device_code.trim().is_empty() || device_code.len() > 512 {
        return Err("GitHub device code is invalid.".to_string());
    }
    let client = http_client()?;
    let response = client
        .post(GITHUB_ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("device_code", device_code.trim()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(|error| github_request_error("complete GitHub authorization", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read GitHub token response: {error}"))?;
    if !status.is_success() {
        return Err(github_response_error(
            "complete GitHub authorization",
            status,
            &body,
        ));
    }
    let token_response: AccessTokenResponse = serde_json::from_str(&body)
        .map_err(|error| format!("invalid GitHub token response: {error}"))?;
    if let Some(error) = token_response.error.as_deref() {
        return match error {
            "authorization_pending" => Ok(PublishingAuthPollResult::Pending {
                interval_seconds: token_response.interval.unwrap_or(5).max(1),
            }),
            "slow_down" => Ok(PublishingAuthPollResult::Pending {
                interval_seconds: token_response.interval.unwrap_or(10).saturating_add(5),
            }),
            "expired_token" => {
                Err("The GitHub authorization code expired. Start again.".to_string())
            }
            "access_denied" => Err("GitHub authorization was denied.".to_string()),
            _ => Err(token_response
                .error_description
                .unwrap_or_else(|| format!("GitHub authorization failed: {error}"))),
        };
    }
    let access_token = token_response
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| "GitHub did not return an access token.".to_string())?;
    if !has_gist_scope(token_response.scope.as_deref()) {
        return Err("GitHub authorization did not grant the required gist scope.".to_string());
    }
    let user = fetch_github_user(&client, &access_token).await?;
    let credential = StoredCredential {
        access_token,
        login: Some(user.login.clone()),
        scope: token_response.scope,
    };
    save_stored_credential(&credential)?;
    Ok(PublishingAuthPollResult::Connected {
        account: PublishingAuthStatus {
            configured: true,
            connected: true,
            login: Some(user.login),
            expires_at: None,
        },
    })
}

#[tauri::command]
pub async fn publishing_auth_disconnect() -> Result<PublishingAuthStatus, String> {
    delete_stored_credential()?;
    auth_status()
}

#[tauri::command]
pub async fn publishing_publish(
    state: tauri::State<'_, AppState>,
    request: PublishPublicationRequest,
) -> Result<PublicationBinding, String> {
    validate_publish_request(&request)?;
    let workspace_root = state.config().workspace_root.clone();
    // A publication id that is already bound must go through sync: creating a
    // second gist here would silently overwrite the binding and orphan the
    // first gist — still live on GitHub (possibly public) with no local record
    // and no way left to manage it from qmux. This is reachable when the
    // binding failed to save after a publish (that path deliberately returns
    // success-with-warning) or when a stale view still shows Publish.
    if let Some(existing) = publication_binding(&workspace_root, &request.publication_id)? {
        return Err(format!(
            "This content is already published at {}. Update the existing publication instead of publishing it again.",
            existing.share_url
        ));
    }
    let token = github_access_token()?
        .ok_or_else(|| "Connect a GitHub account before publishing this content.".to_string())?;
    let client = http_client()?;
    let files = request
        .files
        .iter()
        .map(|(name, content)| {
            (
                name.as_str(),
                CreateGistFile {
                    content: content.as_str(),
                },
            )
        })
        .collect();
    let payload = CreateGistRequest {
        description: format!("{} — published with qmux", request.title.trim()),
        public: request.is_public,
        files,
    };
    let response = client
        .post(format!("{GITHUB_API_BASE}/gists"))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&payload)
        .send()
        .await
        .map_err(|error| github_request_error("publish Gist", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read GitHub Gist response: {error}"))?;
    if !status.is_success() {
        return Err(github_response_error("publish Gist", status, &body));
    }
    let mut gist: GitHubGist = serde_json::from_str(&body)
        .map_err(|error| format!("invalid GitHub Gist response: {error}"))?;
    // The Gist now exists; a missing revision only weakens the next sync's
    // external-edit check, so recover it best-effort instead of failing the
    // publish and losing the local binding.
    if gist.history.is_empty()
        && let Ok(remote) = fetch_gist(&client, &token.access_token, &gist.id).await
    {
        gist = remote.gist;
    }
    let revision = latest_gist_revision(&gist);
    let now = now_millis();
    let mut binding = PublicationBinding {
        publication_id: request.publication_id,
        gist_id: gist.id.clone(),
        gist_url: gist.html_url,
        share_url: format!("{}/p/{}", share_base_url(), gist.id),
        owner_login: gist.owner.map(|owner| owner.login).or(token.login),
        revision,
        is_public: request.is_public,
        source: request.source,
        public_node_ids: request.public_node_ids,
        proposal_states: BTreeMap::new(),
        publication_created_at: publication_created_at(&request.files),
        warning: None,
        created_at: now,
        updated_at: now,
    };
    if let Err(error) = upsert_publication_binding(&workspace_root, &binding) {
        binding.warning = Some(format!(
            "The Gist was created, but qmux could not save its local publication binding: {error}"
        ));
    }
    Ok(binding)
}

#[tauri::command]
pub async fn publishing_sync(
    state: tauri::State<'_, AppState>,
    mut request: SyncPublicationRequest,
) -> Result<PublicationBinding, String> {
    validate_sync_request(&request)?;
    let workspace_root = state.config().workspace_root.clone();
    let existing = publication_binding(&workspace_root, &request.publication_id)?
        .ok_or_else(|| "This publication is no longer linked to a local Gist.".to_string())?;
    if existing.source != request.source {
        return Err("The publication source does not match its saved Gist binding.".to_string());
    }
    if existing.is_public != request.is_public {
        return Err("A Gist's visibility cannot be changed while syncing.".to_string());
    }

    let token = github_access_token()?
        .ok_or_else(|| "Connect a GitHub account before syncing this publication.".to_string())?;
    let client = http_client()?;
    let remote = fetch_gist(&client, &token.access_token, &existing.gist_id).await?;
    let account = fetch_github_user(&client, &token.access_token).await?;
    let owner = remote
        .gist
        .owner
        .as_ref()
        .map(|owner| owner.login.as_str())
        .ok_or_else(|| "GitHub did not identify the owner of this Gist.".to_string())?;
    if owner != account.login {
        return Err(format!(
            "The connected GitHub account @{account} does not own this Gist.",
            account = account.login
        ));
    }
    if remote.gist.public != existing.is_public {
        return Err("The Gist visibility no longer matches the saved publication.".to_string());
    }
    // GitHub occasionally omits the revision history; skip the external-edit
    // check in that case rather than blocking the sync.
    let current_revision = latest_gist_revision(&remote.gist);
    if existing
        .revision
        .as_deref()
        .zip(current_revision.as_deref())
        .is_some_and(|(saved, current)| saved != current)
    {
        return Err(
            "The Gist changed outside qmux after its last sync. Review the Gist before updating it."
                .to_string(),
        );
    }

    let current_index_file = remote
        .gist
        .files
        .get(PUBLICATION_INDEX_FILE)
        .ok_or_else(|| format!("The linked Gist no longer contains {PUBLICATION_INDEX_FILE}."))?;
    let current_index =
        load_gist_file_content(&client, current_index_file, PUBLICATION_INDEX_FILE).await?;
    validate_remote_publication_identity(&current_index, &request.publication_id, &request.source)?;
    preserve_remote_created_at(&mut request.files, &current_index)?;
    validate_sync_request(&request)?;
    let desired_description = format!("{} — published with qmux", request.title.trim());
    let files =
        build_gist_update_files(&client, &remote.gist, &current_index, &request.files).await?;

    let mut gist = if files.is_empty()
        && remote.gist.description.as_deref() == Some(desired_description.as_str())
    {
        remote.gist
    } else {
        let payload = UpdateGistRequest {
            description: (remote.gist.description.as_deref() != Some(desired_description.as_str()))
                .then_some(desired_description),
            files,
        };
        let send_update = |with_precondition: bool| {
            let mut builder = client
                .patch(format!("{GITHUB_API_BASE}/gists/{}", existing.gist_id))
                .header("Accept", "application/vnd.github+json")
                .header("Authorization", format!("Bearer {}", token.access_token))
                .header("User-Agent", GITHUB_USER_AGENT)
                .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
            if with_precondition && let Some(etag) = remote.etag.as_deref() {
                builder = builder.header("If-Match", strong_etag(etag));
            }
            builder.json(&payload).send()
        };
        let mut response = send_update(true)
            .await
            .map_err(|error| github_request_error("sync Gist", error))?;
        // GitHub does not accept every validator it hands out: an If-Match it
        // rejects comes back as a bodyless 400 instead of being evaluated.
        // The revision check above already guards external edits, so retry
        // once without the precondition rather than failing the sync over the
        // narrower in-flight race it covered.
        if response.status() == StatusCode::BAD_REQUEST && remote.etag.is_some() {
            response = send_update(false)
                .await
                .map_err(|error| github_request_error("sync Gist", error))?;
        }
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("failed to read GitHub Gist sync response: {error}"))?;
        if status == StatusCode::PRECONDITION_FAILED {
            return Err(
                "The Gist changed while qmux was preparing the update. Review it and try again."
                    .to_string(),
            );
        }
        if !status.is_success() {
            return Err(github_response_error("sync Gist", status, &body));
        }
        serde_json::from_str(&body)
            .map_err(|error| format!("invalid GitHub Gist sync response: {error}"))?
    };
    // The Gist update already landed; recover a missing revision best-effort
    // instead of failing the sync after the fact.
    if gist.history.is_empty()
        && let Ok(remote) = fetch_gist(&client, &token.access_token, &gist.id).await
    {
        gist = remote.gist;
    }
    let revision = latest_gist_revision(&gist);
    let publication_id = existing.publication_id.clone();
    // Snapshot the states as loaded so the write below can tell which keys
    // this sync actually changed and merge them over the freshest store copy.
    let original_proposal_states = existing.proposal_states.clone();
    let mut proposal_states = existing.proposal_states;
    let mut warning = sync_published_proposal_links(
        &client,
        &token.access_token,
        &gist.id,
        &publication_id,
        &request.public_node_ids,
        &mut proposal_states,
    )
    .await
    .err()
    .map(|error| {
        format!(
            "The publication was updated, but qmux could not link every accepted proposal to its published result: {error}"
        )
    });

    let now = now_millis();
    let template = PublicationBinding {
        publication_id,
        gist_id: gist.id.clone(),
        gist_url: gist.html_url,
        share_url: format!("{}/p/{}", share_base_url(), gist.id),
        owner_login: gist.owner.map(|owner| owner.login).or(Some(account.login)),
        revision,
        is_public: existing.is_public,
        source: request.source,
        public_node_ids: request.public_node_ids,
        proposal_states: proposal_states.clone(),
        publication_created_at: publication_created_at(&request.files),
        warning: warning.clone(),
        created_at: existing.created_at,
        updated_at: now,
    };
    // The sync owns every scalar field of the binding, but proposal states can
    // gain entries while its uploads are in flight (a proposal accepted in the
    // research pane); write those through a merge so they survive.
    let merge_result = update_publication_binding_with(&workspace_root, &template, |stored| {
        let concurrent_states = std::mem::take(&mut stored.proposal_states);
        *stored = template.clone();
        stored.proposal_states = concurrent_states;
        for (key, state) in &proposal_states {
            if original_proposal_states.get(key) != Some(state)
                || !stored.proposal_states.contains_key(key)
            {
                stored.proposal_states.insert(key.clone(), state.clone());
            }
        }
    });
    let binding = match merge_result {
        Ok(binding) => binding,
        Err(error) => {
            append_warning(
                &mut warning,
                format!(
                    "The Gist was updated, but qmux could not save its local publication binding: {error}"
                ),
            );
            let mut binding = template;
            binding.warning = warning;
            binding
        }
    };
    Ok(binding)
}

#[tauri::command(async)]
pub fn publishing_list(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PublicationBinding>, String> {
    Ok(load_publication_store(&state.config().workspace_root)?.bindings)
}

#[tauri::command]
pub async fn publishing_list_proposals(
    state: tauri::State<'_, AppState>,
    publication_id: String,
) -> Result<Vec<PublicationProposal>, String> {
    if !valid_public_identifier(&publication_id) {
        return Err("publicationId has an invalid format.".to_string());
    }
    let workspace_root = state.config().workspace_root.clone();
    let mut binding = publication_binding(&workspace_root, &publication_id)?
        .ok_or_else(|| "This publication is no longer linked to a local Gist.".to_string())?;
    let tree_id = ensure_research_tree_binding(&binding)?;
    let token = github_access_token()?
        .ok_or_else(|| "Connect the GitHub account that owns this publication.".to_string())?;
    let client = http_client()?;
    let remote = fetch_gist(&client, &token.access_token, &binding.gist_id).await?;
    let account = fetch_github_user(&client, &token.access_token).await?;
    ensure_gist_owner(&remote.gist, &account.login)?;
    let comments = fetch_gist_comments(&client, &token.access_token, &binding.gist_id).await?;
    let original_proposal_states = binding.proposal_states.clone();
    let mut changed = reconcile_proposal_node_states(
        state.inner(),
        &mut binding,
        &tree_id,
        &remote.gist,
        &comments,
    )?;
    changed |= reconcile_proposal_resolution_states(&mut binding, &remote.gist, &comments);
    if changed {
        let now = now_millis();
        // Reconcile touches individual proposal states; merge just those over
        // the freshest store copy so a resolve or sync that landed during this
        // listing's GitHub fetches isn't clobbered by a whole-binding write.
        binding = update_publication_binding_with(&workspace_root, &binding, |stored| {
            for (key, state) in &binding.proposal_states {
                if original_proposal_states.get(key) != Some(state)
                    || !stored.proposal_states.contains_key(key)
                {
                    stored.proposal_states.insert(key.clone(), state.clone());
                }
            }
            stored.updated_at = now;
        })?;
    }
    Ok(collect_publication_proposals(
        &binding,
        &remote.gist,
        &comments,
    ))
}

#[tauri::command]
pub async fn publishing_resolve_proposal(
    state: tauri::State<'_, AppState>,
    request: ResolvePublicationProposalRequest,
) -> Result<PublicationBinding, String> {
    if !valid_public_identifier(&request.publication_id) || request.proposal_comment_id == 0 {
        return Err("The publication proposal identifier is invalid.".to_string());
    }
    if request.status != "accepted" && request.status != "declined" {
        return Err("A proposal can only be accepted or declined.".to_string());
    }
    let workspace_root = state.config().workspace_root.clone();
    let mut binding = publication_binding(&workspace_root, &request.publication_id)?
        .ok_or_else(|| "This publication is no longer linked to a local Gist.".to_string())?;
    let tree_id = ensure_research_tree_binding(&binding)?;
    let token = github_access_token()?
        .ok_or_else(|| "Connect the GitHub account that owns this publication.".to_string())?;
    let client = http_client()?;
    let remote = fetch_gist(&client, &token.access_token, &binding.gist_id).await?;
    let account = fetch_github_user(&client, &token.access_token).await?;
    ensure_gist_owner(&remote.gist, &account.login)?;
    let comments = fetch_gist_comments(&client, &token.access_token, &binding.gist_id).await?;
    let proposal_comment = comments
        .iter()
        .find(|comment| comment.id == request.proposal_comment_id)
        .ok_or_else(|| "The proposal comment was not found on GitHub.".to_string())?;
    let proposal = parse_research_proposal(&proposal_comment.body)
        .ok_or_else(|| "The GitHub comment is not a valid qmux research proposal.".to_string())?;
    if proposal.publication_id != request.publication_id {
        return Err("The proposal belongs to a different publication.".to_string());
    }
    let author_login = proposal_comment
        .user
        .as_ref()
        .map(|user| user.login.clone())
        .filter(|login| valid_github_login(login))
        .ok_or_else(|| "GitHub did not identify the proposal author.".to_string())?;
    let parent_node_id = binding
        .public_node_ids
        .iter()
        .find_map(|(private_id, public_id)| {
            (public_id == &proposal.parent_node_id).then(|| private_id.clone())
        })
        .ok_or_else(|| {
            "The proposal targets a research result that is no longer linked.".to_string()
        })?;
    let proposal_digest = research_proposal_digest(&proposal)?;
    let saved_state = binding
        .proposal_states
        .get(&request.proposal_comment_id.to_string())
        .filter(|state| {
            proposal_state_matches(state, request.proposal_comment_id, &author_login, &proposal)
        })
        .cloned();

    let local_node_id = if request.status == "accepted" {
        let local_node_id = request
            .local_node_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                "Accepting a proposal requires the created research node.".to_string()
            })?;
        let local_node = state.research_node(local_node_id)?;
        if local_node.tree_id != tree_id
            || local_node.parent_node_id.as_deref() != Some(parent_node_id.as_str())
            || local_node.prompt != proposal.prompt
            || !local_node
                .publication_proposal
                .as_ref()
                .is_some_and(|reference| {
                    reference.publication_id == request.publication_id
                        && reference.comment_id == request.proposal_comment_id
                })
        {
            return Err("The accepted research node does not match this proposal.".to_string());
        }
        if saved_state
            .as_ref()
            .and_then(|saved| saved.local_node_id.as_deref())
            .is_some_and(|saved_id| saved_id != local_node.id)
        {
            return Err(
                "This proposal is already linked to a different local research node.".to_string(),
            );
        }
        Some(local_node.id)
    } else {
        if request.local_node_id.is_some() {
            return Err("Declined proposals cannot reference a local research node.".to_string());
        }
        if saved_state
            .as_ref()
            .is_some_and(|saved| saved.local_node_id.is_some())
        {
            return Err(
                "This proposal already created a local result; finish accepting it instead."
                    .to_string(),
            );
        }
        None
    };

    let existing_resolution = comments
        .iter()
        .filter_map(|comment| {
            let resolution = parse_proposal_resolution(&comment.body)?;
            let user = comment.user.as_ref()?;
            (user.login == account.login
                && resolution.publication_id == request.publication_id
                && resolution.proposal_comment_id == request.proposal_comment_id
                && resolution.proposal_digest == proposal_digest)
                .then_some((comment.id, resolution))
        })
        .max_by_key(|(comment_id, _)| *comment_id);
    if let Some((_, resolution)) = existing_resolution.as_ref()
        && resolution.status != request.status
    {
        return Err("This proposal already has a different owner resolution.".to_string());
    }
    let existing_resolution_comment_id = existing_resolution
        .as_ref()
        .map(|(comment_id, _)| *comment_id);
    let mut proposal_state = PublicationProposalState {
        proposal_comment_id: request.proposal_comment_id,
        status: request.status.clone(),
        author_login,
        parent_public_node_id: proposal.parent_node_id,
        prompt: proposal.prompt,
        answer_markdown: proposal.answer_markdown,
        anchor: proposal.anchor,
        local_node_id,
        resolution_comment_id: existing_resolution_comment_id,
        published_public_node_id: saved_state.and_then(|saved| {
            (existing_resolution_comment_id.is_some()
                && saved.status == request.status
                && saved.resolution_comment_id == existing_resolution_comment_id)
                .then_some(saved.published_public_node_id)
                .flatten()
        }),
    };
    if request.status == "accepted" && existing_resolution_comment_id.is_none() {
        // Persist the acceptance before posting the resolution comment, but
        // merge into the freshest store copy — a sync finishing during this
        // command's GitHub fetches must not be overwritten wholesale.
        let now = now_millis();
        binding = update_publication_binding_with(&workspace_root, &binding, |stored| {
            stored.proposal_states.insert(
                request.proposal_comment_id.to_string(),
                proposal_state.clone(),
            );
            stored.updated_at = now;
        })?;
    }
    let resolution_comment_id = if let Some(comment_id) = existing_resolution_comment_id {
        comment_id
    } else {
        let body = encode_proposal_resolution(&ProposalResolutionPayload {
            publication_id: request.publication_id.clone(),
            proposal_comment_id: request.proposal_comment_id,
            proposal_digest,
            status: request.status.clone(),
            public_node_id: None,
        })?;
        create_gist_comment(&client, &token.access_token, &binding.gist_id, &body).await?
    };
    proposal_state.resolution_comment_id = Some(resolution_comment_id);
    let now = now_millis();
    let merge_result = update_publication_binding_with(&workspace_root, &binding, |stored| {
        stored.proposal_states.insert(
            request.proposal_comment_id.to_string(),
            proposal_state.clone(),
        );
        stored.warning = None;
        stored.updated_at = now;
    });
    match merge_result {
        Ok(updated) => binding = updated,
        Err(error) => {
            binding
                .proposal_states
                .insert(request.proposal_comment_id.to_string(), proposal_state);
            binding.updated_at = now;
            binding.warning = Some(format!(
                "GitHub recorded the proposal resolution, but qmux could not finish saving its local mapping: {error}"
            ));
        }
    }
    Ok(binding)
}

fn auth_status() -> Result<PublishingAuthStatus, String> {
    let configured = github_client_id().is_some() || environment_credential().is_some();
    let credential = github_access_token()?;
    Ok(PublishingAuthStatus {
        configured,
        connected: credential.is_some(),
        login: credential.and_then(|value| value.login),
        expires_at: None,
    })
}

fn github_client_id() -> Option<String> {
    std::env::var("QMUX_GITHUB_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("QMUX_GITHUB_CLIENT_ID").map(str::to_string))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn share_base_url() -> String {
    std::env::var("QMUX_SHARE_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
        .unwrap_or_else(|| "https://qmux.app".to_string())
}

fn github_access_token() -> Result<Option<StoredCredential>, String> {
    if let Some(credential) = environment_credential() {
        return Ok(Some(credential));
    }
    load_stored_credential()
}

fn environment_credential() -> Option<StoredCredential> {
    std::env::var("QMUX_GITHUB_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|access_token| StoredCredential {
            access_token,
            login: std::env::var("QMUX_GITHUB_LOGIN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            scope: Some("gist".to_string()),
        })
}

#[cfg(target_os = "macos")]
fn keychain_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|error| format!("failed to access macOS Keychain: {error}"))
}

#[cfg(target_os = "macos")]
fn load_stored_credential() -> Result<Option<StoredCredential>, String> {
    match keychain_entry()?.get_password() {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| format!("invalid GitHub credential in macOS Keychain: {error}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "failed to read GitHub credential from macOS Keychain: {error}"
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn load_stored_credential() -> Result<Option<StoredCredential>, String> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn save_stored_credential(credential: &StoredCredential) -> Result<(), String> {
    let raw = serde_json::to_string(credential)
        .map_err(|error| format!("failed to encode GitHub credential: {error}"))?;
    keychain_entry()?
        .set_password(&raw)
        .map_err(|error| format!("failed to save GitHub credential to macOS Keychain: {error}"))
}

#[cfg(not(target_os = "macos"))]
fn save_stored_credential(_credential: &StoredCredential) -> Result<(), String> {
    Err("Secure GitHub credential storage is currently available on macOS only.".to_string())
}

#[cfg(target_os = "macos")]
fn delete_stored_credential() -> Result<(), String> {
    match keychain_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!(
            "failed to remove GitHub credential from macOS Keychain: {error}"
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn delete_stored_credential() -> Result<(), String> {
    Ok(())
}

fn has_gist_scope(scope: Option<&str>) -> bool {
    scope
        .unwrap_or_default()
        .split([',', ' '])
        .map(str::trim)
        .any(|value| value == "gist")
}

fn http_client() -> Result<reqwest::Client, String> {
    crate::ensure_rustls_crypto_provider()?;
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to build GitHub HTTP client: {error}"))
}

async fn fetch_github_user(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<GitHubUser, String> {
    let response = client
        .get(format!("{GITHUB_API_BASE}/user"))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await
        .map_err(|error| github_request_error("load GitHub account", error))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read GitHub account response: {error}"))?;
    if !status.is_success() {
        return Err(github_response_error("load GitHub account", status, &body));
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid GitHub account response: {error}"))
}

async fn fetch_gist(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
) -> Result<RemoteGist, String> {
    let response = client
        .get(format!("{GITHUB_API_BASE}/gists/{gist_id}"))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await
        .map_err(|error| github_request_error("load linked Gist", error))?;
    let status = response.status();
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response
        .text()
        .await
        .map_err(|error| format!("failed to read linked GitHub Gist response: {error}"))?;
    if !status.is_success() {
        return Err(github_response_error("load linked Gist", status, &body));
    }
    let gist = serde_json::from_str(&body)
        .map_err(|error| format!("invalid linked GitHub Gist response: {error}"))?;
    Ok(RemoteGist { gist, etag })
}

fn ensure_research_tree_binding(binding: &PublicationBinding) -> Result<String, String> {
    if binding.source.kind != "researchTree" {
        return Err("Only published research trees accept contributed follow-ups.".to_string());
    }
    binding
        .source
        .detail
        .get("treeId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "The publication is missing its local research tree binding.".to_string())
}

fn ensure_gist_owner(gist: &GitHubGist, login: &str) -> Result<(), String> {
    let owner = gist
        .owner
        .as_ref()
        .map(|owner| owner.login.as_str())
        .ok_or_else(|| "GitHub did not identify the owner of this Gist.".to_string())?;
    if owner == login {
        Ok(())
    } else {
        Err(format!(
            "The connected GitHub account @{login} does not own this Gist."
        ))
    }
}

async fn fetch_gist_comments(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
) -> Result<Vec<GitHubGistComment>, String> {
    let max_pages = MAX_GIST_COMMENTS.div_ceil(GIST_COMMENTS_PER_PAGE);
    let (first_page, last_page) =
        fetch_gist_comments_page(client, access_token, gist_id, 1).await?;
    let page_numbers = if let Some(last_page) = last_page {
        let first = last_page.saturating_sub(max_pages - 1).max(1);
        (first..=last_page).collect::<Vec<_>>()
    } else {
        (1..=max_pages).collect::<Vec<_>>()
    };
    let mut comments = Vec::new();
    for page in page_numbers {
        let mut page_comments = if page == 1 {
            first_page.clone()
        } else {
            fetch_gist_comments_page(client, access_token, gist_id, page)
                .await?
                .0
        };
        let page_len = page_comments.len();
        comments.append(&mut page_comments);
        if (last_page.is_none() && page_len < GIST_COMMENTS_PER_PAGE)
            || comments.len() >= MAX_GIST_COMMENTS
        {
            break;
        }
    }
    comments.truncate(MAX_GIST_COMMENTS);
    Ok(comments)
}

async fn fetch_gist_comments_page(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
    page: usize,
) -> Result<(Vec<GitHubGistComment>, Option<usize>), String> {
    let response = client
        .get(format!(
            "{GITHUB_API_BASE}/gists/{gist_id}/comments?per_page={GIST_COMMENTS_PER_PAGE}&page={page}"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .send()
        .await
        .map_err(|error| github_request_error("load Gist comments", error))?;
    let status = response.status();
    let last_page = response
        .headers()
        .get("link")
        .and_then(|value| value.to_str().ok())
        .and_then(github_last_page);
    let body = read_reqwest_text_limited(
        response,
        MAX_GITHUB_COMMENTS_RESPONSE_BYTES,
        "GitHub comments response",
    )
    .await?;
    if !status.is_success() {
        return Err(github_response_error("load Gist comments", status, &body));
    }
    let comments = serde_json::from_str(&body)
        .map_err(|error| format!("invalid GitHub comments response: {error}"))?;
    Ok((comments, last_page))
}

fn github_last_page(link: &str) -> Option<usize> {
    link.split(',').find_map(|segment| {
        let mut parts = segment.split(';');
        let target = parts.next()?.trim();
        if !parts.any(|value| value.trim() == r#"rel="last""#) {
            return None;
        }
        let url = target.strip_prefix('<')?.strip_suffix('>')?;
        reqwest::Url::parse(url)
            .ok()?
            .query_pairs()
            .find_map(|(key, value)| (key == "page").then(|| value.parse().ok()).flatten())
            .filter(|page| *page > 0)
    })
}

async fn create_gist_comment(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
    body: &str,
) -> Result<u64, String> {
    let response = client
        .post(format!("{GITHUB_API_BASE}/gists/{gist_id}/comments"))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|error| github_request_error("resolve Gist proposal", error))?;
    let status = response.status();
    let response_body = read_reqwest_text_limited(
        response,
        MAX_GITHUB_COMMENTS_RESPONSE_BYTES,
        "GitHub comment response",
    )
    .await?;
    if !status.is_success() {
        return Err(github_response_error(
            "resolve Gist proposal",
            status,
            &response_body,
        ));
    }
    let comment: GitHubGistComment = serde_json::from_str(&response_body)
        .map_err(|error| format!("invalid GitHub comment response: {error}"))?;
    if comment.id == 0 {
        return Err("GitHub returned an invalid proposal resolution comment.".to_string());
    }
    Ok(comment.id)
}

async fn update_gist_comment(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
    comment_id: u64,
    body: &str,
) -> Result<(), String> {
    let response = client
        .patch(format!(
            "{GITHUB_API_BASE}/gists/{gist_id}/comments/{comment_id}"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", GITHUB_USER_AGENT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|error| github_request_error("link resolved Gist proposal", error))?;
    let status = response.status();
    let response_body = read_reqwest_text_limited(
        response,
        MAX_GITHUB_COMMENTS_RESPONSE_BYTES,
        "GitHub comment response",
    )
    .await?;
    if !status.is_success() {
        return Err(github_response_error(
            "link resolved Gist proposal",
            status,
            &response_body,
        ));
    }
    Ok(())
}

async fn sync_published_proposal_links(
    client: &reqwest::Client,
    access_token: &str,
    gist_id: &str,
    publication_id: &str,
    public_node_ids: &BTreeMap<String, String>,
    proposal_states: &mut BTreeMap<String, PublicationProposalState>,
) -> Result<(), String> {
    for state in proposal_states.values_mut() {
        if state.status != "accepted" {
            continue;
        }
        let Some(local_node_id) = state.local_node_id.as_deref() else {
            continue;
        };
        let Some(resolution_comment_id) = state.resolution_comment_id else {
            continue;
        };
        let Some(public_node_id) = public_node_ids.get(local_node_id) else {
            continue;
        };
        if state.published_public_node_id.as_deref() == Some(public_node_id.as_str()) {
            continue;
        }
        let proposal = ResearchProposalPayload {
            publication_id: publication_id.to_string(),
            parent_node_id: state.parent_public_node_id.clone(),
            prompt: state.prompt.clone(),
            answer_markdown: state.answer_markdown.clone(),
            anchor: state.anchor.clone(),
        };
        if !proposal_state_matches(
            state,
            state.proposal_comment_id,
            &state.author_login,
            &proposal,
        ) {
            return Err(format!(
                "saved proposal {} is invalid",
                state.proposal_comment_id
            ));
        }
        let body = encode_proposal_resolution(&ProposalResolutionPayload {
            publication_id: publication_id.to_string(),
            proposal_comment_id: state.proposal_comment_id,
            proposal_digest: research_proposal_digest(&proposal)?,
            status: state.status.clone(),
            public_node_id: Some(public_node_id.clone()),
        })?;
        update_gist_comment(client, access_token, gist_id, resolution_comment_id, &body).await?;
        state.published_public_node_id = Some(public_node_id.clone());
    }
    Ok(())
}

async fn read_reqwest_text_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!("{label} exceeds the {max_bytes} byte limit."));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("failed to read {label}: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("{label} exceeds the {max_bytes} byte limit."));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} is not valid UTF-8."))
}

fn collect_publication_proposals(
    binding: &PublicationBinding,
    gist: &GitHubGist,
    comments: &[GitHubGistComment],
) -> Vec<PublicationProposal> {
    let owner_login = gist.owner.as_ref().map(|owner| owner.login.as_str());
    let private_by_public = binding
        .public_node_ids
        .iter()
        .map(|(private_id, public_id)| (public_id.clone(), private_id.clone()))
        .collect::<HashMap<_, _>>();
    let mut proposals = comments
        .iter()
        .filter_map(|comment| {
            let payload = parse_research_proposal(&comment.body)?;
            if payload.publication_id != binding.publication_id {
                return None;
            }
            let user = comment.user.as_ref()?;
            if !valid_github_login(&user.login) {
                return None;
            }
            let proposal_digest = research_proposal_digest(&payload).ok()?;
            let saved = binding
                .proposal_states
                .get(&comment.id.to_string())
                .filter(|state| proposal_state_matches(state, comment.id, &user.login, &payload));
            let status = owner_login
                .and_then(|owner_login| {
                    latest_proposal_resolution(
                        comments,
                        owner_login,
                        &binding.publication_id,
                        comment.id,
                        &proposal_digest,
                    )
                })
                .map(|(_, resolution)| resolution.status)
                .unwrap_or_else(|| "pending".to_string());
            let local_node_id = (status != "declined")
                .then(|| {
                    saved
                        .filter(|state| state.status == "accepted")
                        .and_then(|state| state.local_node_id.clone())
                })
                .flatten();
            Some(PublicationProposal {
                comment_id: comment.id,
                author_login: user.login.clone(),
                author_url: trusted_github_profile_url(user.html_url.as_deref(), &user.login),
                parent_public_node_id: payload.parent_node_id.clone(),
                parent_node_id: private_by_public
                    .get(payload.parent_node_id.as_str())
                    .map(|value| (*value).to_string()),
                prompt: payload.prompt,
                answer_markdown: payload.answer_markdown,
                anchor: payload.anchor,
                created_at: comment.created_at.clone(),
                status,
                local_node_id,
            })
        })
        .collect::<Vec<_>>();
    proposals.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.comment_id.cmp(&right.comment_id))
    });
    proposals
}

fn proposal_state_matches(
    state: &PublicationProposalState,
    comment_id: u64,
    author_login: &str,
    proposal: &ResearchProposalPayload,
) -> bool {
    state.proposal_comment_id == comment_id
        && (state.status == "accepted" || state.status == "declined")
        && state.author_login == author_login
        && valid_github_login(&state.author_login)
        && state.parent_public_node_id == proposal.parent_node_id
        && valid_public_identifier(&state.parent_public_node_id)
        && state.prompt == proposal.prompt
        && state.answer_markdown == proposal.answer_markdown
        && state.anchor == proposal.anchor
        && state
            .resolution_comment_id
            .is_none_or(|comment_id| comment_id > 0)
        && state
            .published_public_node_id
            .as_deref()
            .is_none_or(valid_public_identifier)
}

fn latest_proposal_resolution(
    comments: &[GitHubGistComment],
    owner_login: &str,
    publication_id: &str,
    proposal_comment_id: u64,
    proposal_digest: &str,
) -> Option<(u64, ProposalResolutionPayload)> {
    comments
        .iter()
        .filter_map(|comment| {
            let resolution = parse_proposal_resolution(&comment.body)?;
            let login = comment.user.as_ref()?.login.as_str();
            (login == owner_login
                && resolution.publication_id == publication_id
                && resolution.proposal_comment_id == proposal_comment_id
                && resolution.proposal_digest == proposal_digest)
                .then_some((comment.id, resolution))
        })
        .max_by_key(|(comment_id, _)| *comment_id)
}

fn reconcile_proposal_node_states(
    state: &AppState,
    binding: &mut PublicationBinding,
    tree_id: &str,
    gist: &GitHubGist,
    comments: &[GitHubGistComment],
) -> Result<bool, String> {
    let detail = state.research_tree(tree_id)?;
    let owner_login = gist.owner.as_ref().map(|owner| owner.login.as_str());
    let private_by_public = binding
        .public_node_ids
        .iter()
        .map(|(private_id, public_id)| (public_id.clone(), private_id.clone()))
        .collect::<HashMap<_, _>>();
    let mut changed = false;
    for comment in comments {
        let Some(proposal) = parse_research_proposal(&comment.body) else {
            continue;
        };
        if proposal.publication_id != binding.publication_id {
            continue;
        }
        let Some(author_login) = comment
            .user
            .as_ref()
            .map(|user| user.login.as_str())
            .filter(|login| valid_github_login(login))
        else {
            continue;
        };
        let Some(parent_node_id) = private_by_public
            .get(proposal.parent_node_id.as_str())
            .map(String::as_str)
        else {
            continue;
        };
        let matching_nodes = detail
            .nodes
            .iter()
            .filter(|node| {
                node.parent_node_id.as_deref() == Some(parent_node_id)
                    && node.prompt == proposal.prompt
                    && node.publication_proposal.as_ref().is_some_and(|reference| {
                        reference.publication_id == binding.publication_id
                            && reference.comment_id == comment.id
                    })
            })
            .collect::<Vec<_>>();
        // Anomalies below are *skipped*, never turned into hard errors: they
        // arise from supported flows (two instances accepting the same
        // proposal, an accept fork surviving a failed resolve after the owner
        // declined elsewhere), and an error here would propagate out of
        // publishing_list_proposals and keep the entire proposals pane failing
        // on every refresh until a node is hand-deleted. Skipping leaves the
        // saved state untouched — the listing still renders the proposal from
        // the comments themselves — and reconciliation resumes on its own if
        // the ambiguity is resolved.
        if matching_nodes.len() > 1 {
            eprintln!(
                "qmux: proposal {} is linked to multiple local research nodes; leaving its saved state unchanged",
                comment.id
            );
            continue;
        }
        let Some(local_node) = matching_nodes.first() else {
            continue;
        };
        let proposal_digest = research_proposal_digest(&proposal)?;
        let resolution = owner_login.and_then(|owner_login| {
            latest_proposal_resolution(
                comments,
                owner_login,
                &binding.publication_id,
                comment.id,
                &proposal_digest,
            )
        });
        if resolution
            .as_ref()
            .is_some_and(|(_, resolution)| resolution.status != "accepted")
        {
            eprintln!(
                "qmux: proposal {} has a local result but an incompatible owner resolution; leaving its saved state unchanged",
                comment.id
            );
            continue;
        }
        let resolution_comment_id = resolution.map(|(comment_id, _)| comment_id);
        let key = comment.id.to_string();
        if let Some(saved) = binding.proposal_states.get_mut(&key) {
            if !proposal_state_matches(saved, comment.id, author_login, &proposal)
                || saved.status != "accepted"
            {
                eprintln!(
                    "qmux: proposal {} has an incompatible saved local mapping; leaving it unchanged",
                    comment.id
                );
                continue;
            }
            if saved
                .local_node_id
                .as_deref()
                .is_some_and(|node_id| node_id != local_node.id)
            {
                eprintln!(
                    "qmux: proposal {} is already linked to a different local research node; leaving it unchanged",
                    comment.id
                );
                continue;
            }
            if saved.local_node_id.as_deref() != Some(local_node.id.as_str())
                || (resolution_comment_id.is_some()
                    && saved.resolution_comment_id != resolution_comment_id)
            {
                saved.local_node_id = Some(local_node.id.clone());
                if resolution_comment_id.is_some() {
                    saved.resolution_comment_id = resolution_comment_id;
                }
                changed = true;
            }
        } else {
            binding.proposal_states.insert(
                key,
                PublicationProposalState {
                    proposal_comment_id: comment.id,
                    status: "accepted".to_string(),
                    author_login: author_login.to_string(),
                    parent_public_node_id: proposal.parent_node_id,
                    prompt: proposal.prompt,
                    answer_markdown: proposal.answer_markdown,
                    anchor: proposal.anchor,
                    local_node_id: Some(local_node.id.clone()),
                    resolution_comment_id,
                    published_public_node_id: None,
                },
            );
            changed = true;
        }
    }
    Ok(changed)
}

fn reconcile_proposal_resolution_states(
    binding: &mut PublicationBinding,
    gist: &GitHubGist,
    comments: &[GitHubGistComment],
) -> bool {
    let Some(owner_login) = gist.owner.as_ref().map(|owner| owner.login.as_str()) else {
        return false;
    };
    let mut changed = false;
    for comment in comments {
        let Some(proposal) = parse_research_proposal(&comment.body) else {
            continue;
        };
        if proposal.publication_id != binding.publication_id {
            continue;
        }
        let Some(author_login) = comment.user.as_ref().map(|user| user.login.as_str()) else {
            continue;
        };
        let Ok(proposal_digest) = research_proposal_digest(&proposal) else {
            continue;
        };
        let Some((resolution_comment_id, resolution)) = latest_proposal_resolution(
            comments,
            owner_login,
            &binding.publication_id,
            comment.id,
            &proposal_digest,
        ) else {
            continue;
        };
        let Some(state) = binding.proposal_states.get_mut(&comment.id.to_string()) else {
            continue;
        };
        if proposal_state_matches(state, comment.id, author_login, &proposal)
            && state.status == resolution.status
            && state.resolution_comment_id != Some(resolution_comment_id)
        {
            state.resolution_comment_id = Some(resolution_comment_id);
            changed = true;
        }
    }
    changed
}

fn parse_research_proposal(body: &str) -> Option<ResearchProposalPayload> {
    let payload: ResearchProposalPayload = parse_comment_marker(body, PROPOSAL_MARKER_PREFIX)?;
    if !valid_public_identifier(&payload.publication_id)
        || !valid_public_identifier(&payload.parent_node_id)
        || payload.prompt.trim().is_empty()
        || payload.prompt.chars().count() > MAX_PROPOSAL_PROMPT_CHARACTERS
        || payload
            .answer_markdown
            .as_deref()
            .is_some_and(|answer| answer.chars().count() > MAX_PROPOSAL_ANSWER_CHARACTERS)
    {
        return None;
    }
    if let Some(anchor) = &payload.anchor {
        if anchor.end <= anchor.start
            || anchor.exact.trim().is_empty()
            || anchor.exact.chars().count() > MAX_PROPOSAL_QUOTE_CHARACTERS
            || anchor.prefix.chars().count() > MAX_PROPOSAL_CONTEXT_CHARACTERS
            || anchor.suffix.chars().count() > MAX_PROPOSAL_CONTEXT_CHARACTERS
        {
            return None;
        }
    }
    Some(ResearchProposalPayload {
        publication_id: payload.publication_id,
        parent_node_id: payload.parent_node_id,
        prompt: payload.prompt.trim().to_string(),
        answer_markdown: payload
            .answer_markdown
            .map(|answer| answer.trim().to_string())
            .filter(|answer| !answer.is_empty()),
        anchor: payload.anchor,
    })
}

fn research_proposal_digest(payload: &ResearchProposalPayload) -> Result<String, String> {
    // Mirrors the TS digest input: anchor-free proposals keep the original
    // four-element array; an anchor appends its five fields.
    let mut input = vec![
        serde_json::Value::from(payload.publication_id.clone()),
        serde_json::Value::from(payload.parent_node_id.clone()),
        serde_json::Value::from(payload.prompt.clone()),
        payload
            .answer_markdown
            .clone()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    ];
    if let Some(anchor) = &payload.anchor {
        input.push(serde_json::Value::from(anchor.start));
        input.push(serde_json::Value::from(anchor.end));
        input.push(serde_json::Value::from(anchor.exact.clone()));
        input.push(serde_json::Value::from(anchor.prefix.clone()));
        input.push(serde_json::Value::from(anchor.suffix.clone()));
    }
    let input = serde_json::to_vec(&input)
        .map_err(|error| format!("failed to encode proposal digest input: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(input)))
}

fn parse_proposal_resolution(body: &str) -> Option<ProposalResolutionPayload> {
    let payload: ProposalResolutionPayload =
        parse_comment_marker(body, PROPOSAL_RESOLUTION_MARKER_PREFIX)?;
    if !valid_public_identifier(&payload.publication_id)
        || payload.proposal_comment_id == 0
        || !valid_sha256(&payload.proposal_digest)
        || (payload.status != "accepted" && payload.status != "declined")
        || payload
            .public_node_id
            .as_deref()
            .is_some_and(|node_id| !valid_public_identifier(node_id))
    {
        return None;
    }
    Some(payload)
}

fn parse_comment_marker<T: for<'de> Deserialize<'de>>(body: &str, prefix: &str) -> Option<T> {
    let encoded = body
        .strip_prefix(prefix)?
        .split_once(COMMENT_MARKER_SUFFIX)?
        .0;
    if encoded.is_empty() || encoded.len() > 100_000 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn encode_proposal_resolution(payload: &ProposalResolutionPayload) -> Result<String, String> {
    let raw = serde_json::to_vec(payload)
        .map_err(|error| format!("failed to encode proposal resolution: {error}"))?;
    let encoded = URL_SAFE_NO_PAD.encode(raw);
    let message = if payload.status == "accepted" {
        "Accepted this follow-up into the owner's qmux research tree."
    } else {
        "The owner declined this follow-up."
    };
    Ok(format!(
        "{PROPOSAL_RESOLUTION_MARKER_PREFIX}{encoded}{COMMENT_MARKER_SUFFIX}\n\n{message}"
    ))
}

fn valid_github_login(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn trusted_github_profile_url(value: Option<&str>, login: &str) -> String {
    let fallback = format!("https://github.com/{login}");
    let Some(value) = value else {
        return fallback;
    };
    let Ok(url) = reqwest::Url::parse(value) else {
        return fallback;
    };
    if url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
    {
        url.to_string()
    } else {
        fallback
    }
}

fn latest_gist_revision(gist: &GitHubGist) -> Option<String> {
    gist.history
        .first()
        .map(|revision| revision.version.clone())
        .filter(|revision| !revision.is_empty())
}

async fn load_gist_file_content(
    client: &reqwest::Client,
    file: &GitHubGistFile,
    label: &str,
) -> Result<String, String> {
    if file.size > MAX_PUBLICATION_FILE_BYTES {
        return Err(format!(
            "{label} exceeds the {MAX_PUBLICATION_FILE_BYTES} byte publication limit."
        ));
    }
    if !file.truncated
        && let Some(content) = file.content.as_deref()
    {
        if content.len() > MAX_PUBLICATION_FILE_BYTES {
            return Err(format!(
                "{label} exceeds the {MAX_PUBLICATION_FILE_BYTES} byte publication limit."
            ));
        }
        return Ok(content.to_string());
    }
    let raw_url = file
        .raw_url
        .as_deref()
        .ok_or_else(|| format!("{label} is truncated and has no raw URL."))?;
    let parsed = reqwest::Url::parse(raw_url)
        .map_err(|error| format!("{label} has an invalid raw URL: {error}"))?;
    validate_github_raw_url(&parsed, label)?;
    let mut response = client
        .get(parsed)
        .header("User-Agent", GITHUB_USER_AGENT)
        .send()
        .await
        .map_err(|error| github_request_error(&format!("load {label}"), error))?;
    validate_github_raw_url(response.url(), label)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "GitHub could not provide {label} (HTTP {}).",
            status.as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PUBLICATION_FILE_BYTES as u64)
    {
        return Err(format!(
            "{label} exceeds the {MAX_PUBLICATION_FILE_BYTES} byte publication limit."
        ));
    }
    let mut bytes = Vec::with_capacity(file.size.min(MAX_PUBLICATION_FILE_BYTES));
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("failed to read {label}: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_PUBLICATION_FILE_BYTES {
            return Err(format!(
                "{label} exceeds the {MAX_PUBLICATION_FILE_BYTES} byte publication limit."
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} is not valid UTF-8."))
}

fn validate_github_raw_url(url: &reqwest::Url, label: &str) -> Result<(), String> {
    let trusted = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("gist.githubusercontent.com" | "raw.githubusercontent.com")
        );
    if trusted {
        Ok(())
    } else {
        Err(format!("{label} has an untrusted raw URL."))
    }
}

async fn build_gist_update_files(
    client: &reqwest::Client,
    gist: &GitHubGist,
    current_index: &str,
    desired_files: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Option<UpdateGistFile>>, String> {
    let current_value: Value = serde_json::from_str(current_index)
        .map_err(|error| format!("The linked {PUBLICATION_INDEX_FILE} is invalid: {error}"))?;
    let desired_index = desired_files
        .get(PUBLICATION_INDEX_FILE)
        .expect("sync request validation requires publication.json");
    let desired_value: Value = serde_json::from_str(desired_index)
        .map_err(|error| format!("{PUBLICATION_INDEX_FILE} is invalid JSON: {error}"))?;
    let current_owned = publication_owned_files(&current_value)?;
    let mut desired_owned = publication_owned_files(&desired_value)?;
    desired_owned.extend(desired_files.keys().cloned());
    let current_hashes = research_file_hashes(&current_value);
    let desired_hashes = research_file_hashes(&desired_value);
    let mut update = BTreeMap::new();

    for (name, desired_content) in desired_files {
        let unchanged = if name == PUBLICATION_INDEX_FILE {
            current_index == desired_content
        } else if desired_hashes.get(name).is_some()
            && desired_hashes.get(name) == current_hashes.get(name)
        {
            true
        } else if let Some(remote_file) = gist.files.get(name) {
            load_gist_file_content(client, remote_file, name).await? == *desired_content
        } else {
            false
        };
        if !unchanged {
            update.insert(
                name.clone(),
                Some(UpdateGistFile {
                    content: desired_content.clone(),
                }),
            );
        }
    }
    for name in current_owned.difference(&desired_owned) {
        update.insert(name.clone(), None);
    }
    Ok(update)
}

fn publication_owned_files(value: &Value) -> Result<HashSet<String>, String> {
    let root = value
        .as_object()
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} must contain an object."))?;
    let mut files = HashSet::from([
        PUBLICATION_INDEX_FILE.to_string(),
        PUBLICATION_README_FILE.to_string(),
    ]);
    match root.get("kind").and_then(Value::as_str) {
        Some("transcript") => {
            let text_file = root
                .get("transcript")
                .and_then(Value::as_object)
                .and_then(|transcript| transcript.get("textFile"))
                .and_then(Value::as_str)
                .filter(|name| valid_publication_filename(name))
                .ok_or_else(|| {
                    format!("{PUBLICATION_INDEX_FILE} has invalid transcript file metadata.")
                })?;
            files.insert(text_file.to_string());
        }
        Some("research-answer" | "research-tree") => {
            let nodes = root
                .get("research")
                .and_then(Value::as_object)
                .and_then(|research| research.get("nodes"))
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    format!("{PUBLICATION_INDEX_FILE} has invalid research metadata.")
                })?;
            for node in nodes {
                let answer_file = node
                    .as_object()
                    .and_then(|item| item.get("answerFile"))
                    .and_then(Value::as_str)
                    .filter(|name| valid_publication_filename(name))
                    .ok_or_else(|| {
                        format!("{PUBLICATION_INDEX_FILE} has invalid research file metadata.")
                    })?;
                files.insert(answer_file.to_string());
            }
        }
        _ => {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} has an unsupported publication kind."
            ));
        }
    }
    Ok(files)
}

fn research_file_hashes(value: &Value) -> HashMap<String, String> {
    value
        .get("research")
        .and_then(Value::as_object)
        .and_then(|research| research.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let item = node.as_object()?;
            Some((
                item.get("answerFile")?.as_str()?.to_string(),
                item.get("contentHash")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

fn validate_remote_publication_identity(
    raw: &str,
    expected_publication_id: &str,
    source: &PublicationSource,
) -> Result<(), String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("The linked {PUBLICATION_INDEX_FILE} is invalid: {error}"))?;
    let root = value
        .as_object()
        .ok_or_else(|| format!("The linked {PUBLICATION_INDEX_FILE} must contain an object."))?;
    if root.get("schemaVersion").and_then(Value::as_u64) != Some(1)
        || root.get("publicationId").and_then(Value::as_str) != Some(expected_publication_id)
    {
        return Err("The linked Gist contains a different qmux publication.".to_string());
    }
    let expected_kind = match source.kind.as_str() {
        "transcript" => "transcript",
        "researchAnswer" => "research-answer",
        "researchTree" => "research-tree",
        _ => return Err("The saved publication source is unsupported.".to_string()),
    };
    if root.get("kind").and_then(Value::as_str) != Some(expected_kind) {
        return Err("The linked Gist publication type no longer matches qmux.".to_string());
    }
    Ok(())
}

fn publication_created_at(files: &BTreeMap<String, String>) -> Option<String> {
    serde_json::from_str::<Value>(files.get(PUBLICATION_INDEX_FILE)?)
        .ok()?
        .get("createdAt")?
        .as_str()
        .map(str::to_string)
}

fn preserve_remote_created_at(
    files: &mut BTreeMap<String, String>,
    current_index: &str,
) -> Result<(), String> {
    let current: Value = serde_json::from_str(current_index)
        .map_err(|error| format!("The linked {PUBLICATION_INDEX_FILE} is invalid: {error}"))?;
    let created_at = current
        .get("createdAt")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .ok_or_else(|| {
            format!("The linked {PUBLICATION_INDEX_FILE} has an invalid createdAt value.")
        })?;
    let desired_raw = files
        .get(PUBLICATION_INDEX_FILE)
        .expect("sync request validation requires publication.json");
    let mut desired: Value = serde_json::from_str(desired_raw)
        .map_err(|error| format!("{PUBLICATION_INDEX_FILE} is invalid JSON: {error}"))?;
    if desired.get("createdAt").and_then(Value::as_str) == Some(created_at) {
        return Ok(());
    }
    let root = desired
        .as_object_mut()
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} must contain an object."))?;
    root.insert(
        "createdAt".to_string(),
        Value::String(created_at.to_string()),
    );
    root.remove("contentHash");
    let hash = format!("{:x}", Sha256::digest(canonical_json(&desired).as_bytes()));
    desired
        .as_object_mut()
        .expect("publication root checked above")
        .insert("contentHash".to_string(), Value::String(hash));
    let encoded = serde_json::to_string_pretty(&desired)
        .map_err(|error| format!("failed to encode {PUBLICATION_INDEX_FILE}: {error}"))?;
    files.insert(PUBLICATION_INDEX_FILE.to_string(), format!("{encoded}\n"));
    Ok(())
}

fn github_request_error(action: &str, error: reqwest::Error) -> String {
    if error.is_timeout() {
        format!("GitHub timed out while trying to {action}.")
    } else {
        format!("Failed to {action}: {error}")
    }
}

/// GitHub's REST API serves weak validators (`W/"..."`) on gist reads, but a
/// weak entity-tag is invalid in If-Match (RFC 9110 §13.1.1), which GitHub
/// rejects with a bodyless 400 rather than evaluating. Send the strong form
/// of the tag it gave us.
fn strong_etag(value: &str) -> &str {
    value.trim().trim_start_matches("W/")
}

fn github_response_error(action: &str, status: StatusCode, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("GitHub request failed")
                .to_string()
        });
    match status {
        StatusCode::UNAUTHORIZED => {
            "GitHub rejected the saved credential. Disconnect and connect the account again."
                .to_string()
        }
        StatusCode::FORBIDDEN => format!("GitHub refused to {action}: {detail}"),
        _ => format!("Failed to {action} (HTTP {}): {detail}", status.as_u16()),
    }
}

fn validate_publish_request(request: &PublishPublicationRequest) -> Result<(), String> {
    validate_publication_payload(
        &request.publication_id,
        &request.title,
        &request.files,
        &request.public_node_ids,
    )
}

fn validate_sync_request(request: &SyncPublicationRequest) -> Result<(), String> {
    validate_publication_payload(
        &request.publication_id,
        &request.title,
        &request.files,
        &request.public_node_ids,
    )
}

fn validate_publication_payload(
    publication_id: &str,
    title: &str,
    files: &BTreeMap<String, String>,
    public_node_ids: &BTreeMap<String, String>,
) -> Result<(), String> {
    if !valid_public_identifier(publication_id) {
        return Err("publicationId has an invalid format.".to_string());
    }
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 240 {
        return Err("Publication title must contain 1 to 240 characters.".to_string());
    }
    if files.is_empty() || files.len() > MAX_PUBLICATION_FILES {
        return Err(format!(
            "A publication must contain between 1 and {MAX_PUBLICATION_FILES} files."
        ));
    }
    if !files.contains_key(PUBLICATION_INDEX_FILE) || !files.contains_key(PUBLICATION_README_FILE) {
        return Err(format!(
            "A publication must contain {PUBLICATION_INDEX_FILE} and {PUBLICATION_README_FILE}."
        ));
    }
    let mut total_bytes = 0usize;
    for (name, content) in files {
        if !valid_publication_filename(name) {
            return Err(format!("Publication filename {name:?} is invalid."));
        }
        let bytes = content.len();
        if bytes > MAX_PUBLICATION_FILE_BYTES {
            return Err(format!(
                "{name} exceeds the {MAX_PUBLICATION_FILE_BYTES} byte publication limit."
            ));
        }
        total_bytes = total_bytes.saturating_add(bytes);
    }
    if total_bytes > MAX_PUBLICATION_TOTAL_BYTES {
        return Err(format!(
            "Publication exceeds the {MAX_PUBLICATION_TOTAL_BYTES} byte total limit."
        ));
    }
    let mut seen_public_node_ids = HashSet::new();
    for (private_id, public_id) in public_node_ids {
        if private_id.trim().is_empty() || private_id.len() > 256 {
            return Err(
                "A private research node ID in the publication binding is invalid.".to_string(),
            );
        }
        if !valid_public_identifier(public_id) || !seen_public_node_ids.insert(public_id) {
            return Err(
                "A public research node ID in the publication binding is invalid.".to_string(),
            );
        }
    }
    let index = files
        .get(PUBLICATION_INDEX_FILE)
        .expect("required publication index checked above");
    validate_publication_index(index, publication_id, title, files)
}

fn validate_publication_index(
    raw: &str,
    expected_publication_id: &str,
    expected_title: &str,
    files: &BTreeMap<String, String>,
) -> Result<(), String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| format!("{PUBLICATION_INDEX_FILE} is invalid JSON: {error}"))?;
    let root = value
        .as_object()
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} must contain an object."))?;
    if root.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} has an unsupported schema version."
        ));
    }
    if root.get("publicationId").and_then(Value::as_str) != Some(expected_publication_id) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} does not match the publication being published."
        ));
    }
    if root.get("title").and_then(Value::as_str) != Some(expected_title) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} does not match the publication title."
        ));
    }
    let supplied_hash = root
        .get("contentHash")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing contentHash."))?;
    let mut unhashed = value.clone();
    unhashed
        .as_object_mut()
        .expect("publication root checked above")
        .remove("contentHash");
    let expected_hash = format!("{:x}", Sha256::digest(canonical_json(&unhashed).as_bytes()));
    if supplied_hash != expected_hash {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} has an invalid content hash."
        ));
    }
    match root.get("kind").and_then(Value::as_str) {
        Some("transcript") => validate_transcript_index(root, files),
        Some("research-answer") | Some("research-tree") => validate_research_index(root, files),
        _ => Err(format!(
            "{PUBLICATION_INDEX_FILE} has an unsupported publication kind."
        )),
    }
}

fn validate_transcript_index(
    root: &serde_json::Map<String, Value>,
    files: &BTreeMap<String, String>,
) -> Result<(), String> {
    let transcript = root
        .get("transcript")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing transcript metadata."))?;
    let text_file = transcript
        .get("textFile")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing transcript.textFile."))?;
    if text_file != "transcript.txt" || !files.contains_key(text_file) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} must reference the included transcript.txt file."
        ));
    }
    let messages = transcript
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing transcript.messages."))?;
    if messages.len() > 10_000 {
        return Err(
            "A transcript publication cannot contain more than 10,000 messages.".to_string(),
        );
    }
    for (index, message) in messages.iter().enumerate() {
        let item = message.as_object().ok_or_else(|| {
            format!("{PUBLICATION_INDEX_FILE} transcript message {index} must be an object.")
        })?;
        let role = item.get("role").and_then(Value::as_str);
        if role != Some("user") && role != Some("assistant") {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} transcript message {index} has an invalid role."
            ));
        }
        require_bounded_string(item.get("id"), 256, "transcript message id", false)?;
        require_bounded_string(item.get("label"), 120, "transcript message label", false)?;
        require_bounded_string(
            item.get("text"),
            MAX_PUBLICATION_FILE_BYTES,
            "transcript message text",
            false,
        )?;
    }
    Ok(())
}

fn validate_research_index(
    root: &serde_json::Map<String, Value>,
    files: &BTreeMap<String, String>,
) -> Result<(), String> {
    let research = root
        .get("research")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing research metadata."))?;
    let root_node_id = research
        .get("rootNodeId")
        .and_then(Value::as_str)
        .filter(|value| valid_public_identifier(value))
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} has an invalid research rootNodeId."))?;
    let selected_node_id = match research.get("selectedNodeId") {
        Some(Value::Null) => None,
        Some(Value::String(value)) if valid_public_identifier(value) => Some(value.as_str()),
        _ => {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} has an invalid research selectedNodeId."
            ));
        }
    };
    let nodes = research
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} is missing research.nodes."))?;
    if nodes.is_empty() || nodes.len() > MAX_PUBLICATION_FILES - 2 {
        return Err(format!(
            "A research publication must contain between 1 and {} nodes.",
            MAX_PUBLICATION_FILES - 2
        ));
    }

    let mut node_ids = HashSet::new();
    let mut parent_by_id = HashMap::<String, Option<String>>::new();
    for (index, node) in nodes.iter().enumerate() {
        let item = node.as_object().ok_or_else(|| {
            format!("{PUBLICATION_INDEX_FILE} research node {index} must be an object.")
        })?;
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| valid_public_identifier(value))
            .ok_or_else(|| {
                format!("{PUBLICATION_INDEX_FILE} research node {index} has an invalid id.")
            })?;
        if !node_ids.insert(id.to_string()) {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} contains duplicate research node ids."
            ));
        }
        let parent_id = match item.get("parentId") {
            Some(Value::Null) => None,
            Some(Value::String(value)) if valid_public_identifier(value) => Some(value.clone()),
            _ => {
                return Err(format!(
                    "{PUBLICATION_INDEX_FILE} research node {id} has an invalid parentId."
                ));
            }
        };
        parent_by_id.insert(id.to_string(), parent_id);
        if !matches!(
            item.get("kind").and_then(Value::as_str),
            Some("run" | "document" | "conversation")
        ) {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} has an invalid kind."
            ));
        }
        require_bounded_string(item.get("title"), 240, "research node title", false)?;
        require_bounded_string(
            item.get("prompt"),
            MAX_PUBLICATION_FILE_BYTES,
            "research node prompt",
            true,
        )?;
        let answer_file = item
            .get("answerFile")
            .and_then(Value::as_str)
            .filter(|value| valid_publication_filename(value))
            .ok_or_else(|| {
                format!("{PUBLICATION_INDEX_FILE} research node {id} has an invalid answerFile.")
            })?;
        let answer = files.get(answer_file).ok_or_else(|| {
            format!("{PUBLICATION_INDEX_FILE} references missing research file {answer_file}.")
        })?;
        let content_hash = item
            .get("contentHash")
            .and_then(Value::as_str)
            .filter(|value| valid_sha256(value))
            .ok_or_else(|| {
                format!("{PUBLICATION_INDEX_FILE} research node {id} has an invalid contentHash.")
            })?;
        let actual_hash = format!("{:x}", Sha256::digest(answer.as_bytes()));
        if content_hash != actual_hash {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} does not match {answer_file}."
            ));
        }
        match item.get("responseRevision") {
            Some(Value::Null) => {}
            Some(Value::String(value)) if valid_sha256(value) => {}
            _ => {
                return Err(format!(
                    "{PUBLICATION_INDEX_FILE} research node {id} has an invalid responseRevision."
                ));
            }
        }
        if !matches!(
            item.get("status").and_then(Value::as_str),
            Some("complete" | "failed" | "cancelled")
        ) {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} has an invalid status."
            ));
        }
        if !item
            .get("createdAt")
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} has an invalid createdAt."
            ));
        }
        if let Some(contribution) = item.get("contribution")
            && !contribution.is_null()
        {
            let contribution = contribution.as_object().ok_or_else(|| {
                format!("{PUBLICATION_INDEX_FILE} research node {id} has an invalid contribution.")
            })?;
            if !contribution
                .get("githubLogin")
                .and_then(Value::as_str)
                .is_some_and(valid_github_login)
                || !contribution
                    .get("proposalCommentId")
                    .and_then(Value::as_u64)
                    .is_some_and(|comment_id| comment_id > 0)
            {
                return Err(format!(
                    "{PUBLICATION_INDEX_FILE} research node {id} has an invalid contribution."
                ));
            }
        }
    }

    if !node_ids.contains(root_node_id) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} research rootNodeId is not present."
        ));
    }
    if selected_node_id.is_some_and(|id| !node_ids.contains(id)) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} research selectedNodeId is not present."
        ));
    }
    if parent_by_id.get(root_node_id) != Some(&None) {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} research root must have a null parent."
        ));
    }
    for (id, parent_id) in &parent_by_id {
        if id == root_node_id {
            continue;
        }
        let Some(parent_id) = parent_id else {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} is disconnected from the root."
            ));
        };
        if !node_ids.contains(parent_id) {
            return Err(format!(
                "{PUBLICATION_INDEX_FILE} research node {id} has an unknown parent."
            ));
        }
    }
    let mut children_by_parent = HashMap::<String, Vec<String>>::new();
    for (id, parent_id) in &parent_by_id {
        if let Some(parent_id) = parent_id {
            children_by_parent
                .entry(parent_id.clone())
                .or_default()
                .push(id.clone());
        }
    }
    let mut reachable = HashSet::new();
    let mut pending = vec![root_node_id.to_string()];
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&id) {
            pending.extend(children.iter().cloned());
        }
    }
    if reachable.len() != node_ids.len() {
        return Err(format!(
            "{PUBLICATION_INDEX_FILE} contains research nodes disconnected from the root."
        ));
    }
    Ok(())
}

fn require_bounded_string(
    value: Option<&Value>,
    max_bytes: usize,
    label: &str,
    allow_empty: bool,
) -> Result<(), String> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{PUBLICATION_INDEX_FILE} has an invalid {label}."))?;
    if (!allow_empty && value.is_empty()) || value.len() > max_bytes {
        return Err(format!("{PUBLICATION_INDEX_FILE} has an invalid {label}."));
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_public_identifier(value: &str) -> bool {
    (8..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_publication_filename(value: &str) -> bool {
    (1..=120).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-'
        })
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).expect("JSON string serialization"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let entries = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key serialization"),
                        canonical_json(&values[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", entries.join(","))
        }
    }
}

fn publication_store_version() -> u32 {
    1
}

fn publications_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(persistence::STATE_DIR)
        .join(PUBLICATIONS_FILE)
}

fn load_publication_store(workspace_root: &Path) -> Result<PublicationStore, String> {
    let path = publications_path(workspace_root);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PublicationStore {
                version: publication_store_version(),
                bindings: Vec::new(),
            });
        }
        Err(error) => {
            return Err(format!(
                "failed to read publication bindings {}: {error}",
                path.display()
            ));
        }
    };
    let store: PublicationStore = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid publication bindings {}: {error}", path.display()))?;
    if store.version != publication_store_version() {
        return Err(format!(
            "unsupported publication bindings version {}",
            store.version
        ));
    }
    Ok(store)
}

fn publication_binding(
    workspace_root: &Path,
    publication_id: &str,
) -> Result<Option<PublicationBinding>, String> {
    Ok(load_publication_store(workspace_root)?
        .bindings
        .into_iter()
        .find(|binding| binding.publication_id == publication_id))
}

fn upsert_publication_binding(
    workspace_root: &Path,
    binding: &PublicationBinding,
) -> Result<(), String> {
    update_publication_binding_with(workspace_root, binding, |stored| {
        *stored = binding.clone();
    })
    .map(|_| ())
}

/// Applies a command's changes to a binding against the *current* store state
/// instead of overwriting it with the command's stale snapshot. Commands load
/// a binding and then hold it across long GitHub round-trips; a concurrent
/// command can persist changes in that window (a proposal resolve during a
/// sync's uploads, a sync during a listing's reconcile), and a whole-binding
/// write-back would silently discard them until the next reconcile rebuilt
/// what it could. `apply` receives the freshest stored copy (or `fallback`
/// when the binding is no longer stored) and mutates only what the command
/// owns; the merged result is persisted and returned.
fn update_publication_binding_with(
    workspace_root: &Path,
    fallback: &PublicationBinding,
    apply: impl FnOnce(&mut PublicationBinding),
) -> Result<PublicationBinding, String> {
    let _guard = PUBLICATIONS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = load_publication_store(workspace_root)?;
    let position = store
        .bindings
        .iter()
        .position(|existing| existing.publication_id == fallback.publication_id);
    let mut binding = match position {
        Some(position) => store.bindings.remove(position),
        None => fallback.clone(),
    };
    apply(&mut binding);
    store.bindings.push(binding.clone());
    store
        .bindings
        .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    save_publication_store(workspace_root, &store)?;
    Ok(binding)
}

fn save_publication_store(workspace_root: &Path, store: &PublicationStore) -> Result<(), String> {
    let path = publications_path(workspace_root);
    let parent = path
        .parent()
        .ok_or_else(|| "publication bindings path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create publication bindings directory {}: {error}",
            parent.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let raw = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("failed to encode publication bindings: {error}"))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    persistence::write_synced(&tmp, &raw)
        .map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {error}", path.display())
    })?;
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn default_poll_interval() -> u64 {
    5
}

fn append_warning(warning: &mut Option<String>, message: String) {
    if let Some(existing) = warning {
        existing.push(' ');
        existing.push_str(&message);
    } else {
        *warning = Some(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-publishing-{nanos}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_proposal_state(comment_id: u64, node: Option<&str>) -> PublicationProposalState {
        PublicationProposalState {
            proposal_comment_id: comment_id,
            status: "accepted".to_string(),
            author_login: "contributor".to_string(),
            parent_public_node_id: "node_parent12".to_string(),
            prompt: "Question".to_string(),
            answer_markdown: None,
            anchor: None,
            local_node_id: node.map(str::to_string),
            resolution_comment_id: None,
            published_public_node_id: None,
        }
    }

    fn sample_binding(updated_at: u64) -> PublicationBinding {
        PublicationBinding {
            publication_id: "pub_12345678".to_string(),
            gist_id: "gist123".to_string(),
            gist_url: "https://gist.github.com/owner/gist123".to_string(),
            share_url: "https://qmux.app/p/gist123".to_string(),
            owner_login: Some("owner".to_string()),
            revision: Some("rev1".to_string()),
            is_public: false,
            source: PublicationSource {
                kind: "research-tree".to_string(),
                detail: BTreeMap::new(),
            },
            public_node_ids: BTreeMap::new(),
            proposal_states: BTreeMap::new(),
            publication_created_at: None,
            warning: None,
            created_at: 1,
            updated_at,
        }
    }

    // The store can change while a command holds its loaded binding across
    // GitHub round-trips. The merge write-back must keep concurrent proposal
    // states (a resolve landing mid-sync) while the command's own changes win.
    #[test]
    fn binding_updates_merge_over_concurrent_store_changes() {
        let workspace = temp_workspace();
        // The store as a sync command loaded it: one accepted proposal.
        let mut loaded = sample_binding(10);
        loaded
            .proposal_states
            .insert("1".to_string(), sample_proposal_state(1, None));
        upsert_publication_binding(&workspace, &loaded).unwrap();

        // While the sync's uploads run, a resolve links proposal 1 to a local
        // node and accepts a brand-new proposal 2.
        let mut concurrent = loaded.clone();
        concurrent.proposal_states.insert(
            "1".to_string(),
            sample_proposal_state(1, Some("node_local1")),
        );
        concurrent.proposal_states.insert(
            "2".to_string(),
            sample_proposal_state(2, Some("node_local2")),
        );
        concurrent.updated_at = 20;
        upsert_publication_binding(&workspace, &concurrent).unwrap();

        // The sync writes back: it changed the revision but none of the
        // proposal states it loaded — the same merge shape publishing_sync
        // uses must preserve both concurrent updates.
        let mut template = loaded.clone();
        template.revision = Some("rev2".to_string());
        template.updated_at = 30;
        let original_states = loaded.proposal_states.clone();
        let synced_states = template.proposal_states.clone();
        let merged = update_publication_binding_with(&workspace, &template, |stored| {
            let concurrent_states = std::mem::take(&mut stored.proposal_states);
            *stored = template.clone();
            stored.proposal_states = concurrent_states;
            for (key, state) in &synced_states {
                if original_states.get(key) != Some(state)
                    || !stored.proposal_states.contains_key(key)
                {
                    stored.proposal_states.insert(key.clone(), state.clone());
                }
            }
        })
        .unwrap();

        assert_eq!(merged.revision.as_deref(), Some("rev2"));
        assert_eq!(merged.proposal_states.len(), 2);
        assert_eq!(
            merged.proposal_states["1"].local_node_id.as_deref(),
            Some("node_local1")
        );
        assert_eq!(
            merged.proposal_states["2"].local_node_id.as_deref(),
            Some("node_local2")
        );
        let stored = publication_binding(&workspace, "pub_12345678")
            .unwrap()
            .unwrap();
        assert_eq!(stored.proposal_states.len(), 2);
        assert_eq!(stored.revision.as_deref(), Some("rev2"));
    }

    // A key-scoped update (a resolve) must overlay the freshest stored copy,
    // not resurrect the stale binding it loaded before its network calls.
    #[test]
    fn key_scoped_binding_updates_keep_fresh_scalar_fields() {
        let workspace = temp_workspace();
        let loaded = sample_binding(10);
        upsert_publication_binding(&workspace, &loaded).unwrap();

        // A sync completes while the resolve holds its stale copy.
        let mut concurrent = loaded.clone();
        concurrent.revision = Some("rev2".to_string());
        concurrent.updated_at = 20;
        upsert_publication_binding(&workspace, &concurrent).unwrap();

        let merged = update_publication_binding_with(&workspace, &loaded, |stored| {
            stored
                .proposal_states
                .insert("3".to_string(), sample_proposal_state(3, None));
            stored.updated_at = 30;
        })
        .unwrap();

        assert_eq!(merged.revision.as_deref(), Some("rev2"));
        assert_eq!(merged.updated_at, 30);
        assert!(merged.proposal_states.contains_key("3"));
    }

    fn valid_request() -> PublishPublicationRequest {
        let index = r#"{
  "schemaVersion": 1,
  "publicationId": "pub_12345678",
  "kind": "transcript",
  "title": "Test",
  "createdAt": "2026-07-16T12:00:00.000Z",
  "updatedAt": "2026-07-16T12:00:00.000Z",
  "contentHash": "5f70c4ffb7f0fcbdb0791011aa88513de9cf613c40addbc5c46ad4666bf905fa",
  "transcript": {
    "textFile": "transcript.txt",
    "messages": []
  }
}"#;
        PublishPublicationRequest {
            publication_id: "pub_12345678".to_string(),
            title: "Test".to_string(),
            is_public: false,
            files: BTreeMap::from([
                (PUBLICATION_INDEX_FILE.to_string(), index.to_string()),
                (PUBLICATION_README_FILE.to_string(), "# Test\n".to_string()),
                ("transcript.txt".to_string(), "You\n\nTest\n".to_string()),
            ]),
            source: PublicationSource {
                kind: "transcript".to_string(),
                detail: BTreeMap::new(),
            },
            public_node_ids: BTreeMap::new(),
        }
    }

    fn research_request() -> PublishPublicationRequest {
        let root_file = "# Root\n\n## Answer\n\nRoot answer.\n";
        let child_file = "# Child\n\n## Answer\n\nChild answer.\n";
        let mut publication = serde_json::json!({
            "schemaVersion": 1,
            "publicationId": "pub_research123",
            "kind": "research-tree",
            "title": "Research",
            "createdAt": "2026-07-16T12:00:00.000Z",
            "updatedAt": "2026-07-16T12:00:00.000Z",
            "research": {
                "rootNodeId": "node_root1234",
                "selectedNodeId": "node_child123",
                "nodes": [
                    {
                        "id": "node_root1234",
                        "parentId": null,
                        "kind": "run",
                        "title": "Root",
                        "prompt": "Root question",
                        "answerFile": "node_root1234.md",
                        "contentHash": format!("{:x}", Sha256::digest(root_file.as_bytes())),
                        "responseRevision": "a".repeat(64),
                        "status": "complete",
                        "createdAt": 1
                    },
                    {
                        "id": "node_child123",
                        "parentId": "node_root1234",
                        "kind": "run",
                        "title": "Child",
                        "prompt": "Child question",
                        "answerFile": "node_child123.md",
                        "contentHash": format!("{:x}", Sha256::digest(child_file.as_bytes())),
                        "responseRevision": "b".repeat(64),
                        "status": "complete",
                        "createdAt": 2
                    }
                ]
            }
        });
        rehash_publication(&mut publication);
        PublishPublicationRequest {
            publication_id: "pub_research123".to_string(),
            title: "Research".to_string(),
            is_public: false,
            files: BTreeMap::from([
                (
                    PUBLICATION_INDEX_FILE.to_string(),
                    serde_json::to_string_pretty(&publication).unwrap(),
                ),
                (
                    PUBLICATION_README_FILE.to_string(),
                    "# Research\n".to_string(),
                ),
                ("node_root1234.md".to_string(), root_file.to_string()),
                ("node_child123.md".to_string(), child_file.to_string()),
            ]),
            source: PublicationSource {
                kind: "researchTree".to_string(),
                detail: BTreeMap::new(),
            },
            public_node_ids: BTreeMap::new(),
        }
    }

    fn rehash_publication(publication: &mut Value) {
        publication.as_object_mut().unwrap().remove("contentHash");
        let hash = format!(
            "{:x}",
            Sha256::digest(canonical_json(publication).as_bytes())
        );
        publication
            .as_object_mut()
            .unwrap()
            .insert("contentHash".to_string(), Value::String(hash));
    }

    #[test]
    fn validates_a_matching_publication_index() {
        validate_publish_request(&valid_request()).unwrap();
    }

    #[test]
    fn rejects_traversal_filenames() {
        let mut request = valid_request();
        request
            .files
            .insert("../secret".to_string(), "no".to_string());
        assert!(validate_publish_request(&request).is_err());
    }

    #[test]
    fn validates_research_topology_and_file_hashes() {
        validate_publish_request(&research_request()).unwrap();
    }

    #[test]
    fn rejects_research_nodes_disconnected_from_the_root() {
        let mut request = research_request();
        let mut publication: Value =
            serde_json::from_str(&request.files[PUBLICATION_INDEX_FILE]).unwrap();
        publication["research"]["nodes"][1]["parentId"] = Value::Null;
        rehash_publication(&mut publication);
        request.files.insert(
            PUBLICATION_INDEX_FILE.to_string(),
            serde_json::to_string_pretty(&publication).unwrap(),
        );
        assert!(
            validate_publish_request(&request)
                .unwrap_err()
                .contains("disconnected")
        );
    }

    #[test]
    fn sync_plan_updates_metadata_and_deletes_only_removed_publication_files() {
        let current = research_request();
        let mut desired = research_request();
        let mut publication: Value =
            serde_json::from_str(&desired.files[PUBLICATION_INDEX_FILE]).unwrap();
        publication["updatedAt"] = Value::String("2026-07-16T13:00:00.000Z".to_string());
        publication["research"]["selectedNodeId"] = Value::String("node_root1234".to_string());
        publication["research"]["nodes"]
            .as_array_mut()
            .unwrap()
            .truncate(1);
        rehash_publication(&mut publication);
        desired.files.insert(
            PUBLICATION_INDEX_FILE.to_string(),
            serde_json::to_string_pretty(&publication).unwrap(),
        );
        desired.files.insert(
            PUBLICATION_README_FILE.to_string(),
            "# Research updated\n".to_string(),
        );
        desired.files.remove("node_child123.md");
        let mut gist = GitHubGist {
            id: "gist12345".to_string(),
            html_url: "https://gist.github.com/gist12345".to_string(),
            description: Some("Research — published with qmux".to_string()),
            public: false,
            files: current
                .files
                .iter()
                .map(|(name, content)| {
                    (
                        name.clone(),
                        GitHubGistFile {
                            size: content.len(),
                            truncated: false,
                            content: Some(content.clone()),
                            raw_url: None,
                        },
                    )
                })
                .collect(),
            owner: Some(GitHubUser {
                login: "owner".to_string(),
            }),
            history: vec![GitHubGistRevision {
                version: "a".repeat(40),
            }],
        };
        gist.files.insert(
            "unrelated.txt".to_string(),
            GitHubGistFile {
                size: 4,
                truncated: false,
                content: Some("keep".to_string()),
                raw_url: None,
            },
        );
        let client = http_client().unwrap();
        let update = tauri::async_runtime::block_on(build_gist_update_files(
            &client,
            &gist,
            &current.files[PUBLICATION_INDEX_FILE],
            &desired.files,
        ))
        .unwrap();

        assert!(update.contains_key(PUBLICATION_INDEX_FILE));
        assert!(update.contains_key(PUBLICATION_README_FILE));
        assert!(!update.contains_key("node_root1234.md"));
        assert!(matches!(update.get("node_child123.md"), Some(None)));
        assert!(!update.contains_key("unrelated.txt"));
    }

    #[test]
    fn sync_preserves_the_remote_publication_creation_time() {
        let current = research_request();
        let mut desired = research_request();
        let mut publication: Value =
            serde_json::from_str(&desired.files[PUBLICATION_INDEX_FILE]).unwrap();
        publication["createdAt"] = Value::String("2026-07-16T12:00:05.000Z".to_string());
        rehash_publication(&mut publication);
        desired.files.insert(
            PUBLICATION_INDEX_FILE.to_string(),
            serde_json::to_string_pretty(&publication).unwrap(),
        );

        preserve_remote_created_at(&mut desired.files, &current.files[PUBLICATION_INDEX_FILE])
            .unwrap();
        validate_publish_request(&desired).unwrap();
        let preserved: Value =
            serde_json::from_str(&desired.files[PUBLICATION_INDEX_FILE]).unwrap();
        assert_eq!(
            preserved["createdAt"],
            Value::String("2026-07-16T12:00:00.000Z".to_string())
        );
    }

    #[test]
    fn parses_contributed_follow_up_and_owner_resolution_comments() {
        let proposal = ResearchProposalPayload {
            publication_id: "pub_research123".to_string(),
            parent_node_id: "node_root1234".to_string(),
            prompt: "Which evidence would change the answer?".to_string(),
            answer_markdown: Some("A proposed answer.".to_string()),
            anchor: None,
        };
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&proposal).unwrap());
        let body =
            format!("{PROPOSAL_MARKER_PREFIX}{encoded}{COMMENT_MARKER_SUFFIX}\n\nVisible proposal");
        assert_eq!(
            parse_research_proposal(&body).unwrap().prompt,
            proposal.prompt
        );
        let proposal_digest = research_proposal_digest(&proposal).unwrap();
        assert_eq!(
            proposal_digest,
            "21624f06261ded78175de979485cc047abfe4fd1508fa3009b4f3cfd2de3a9d9"
        );

        let resolution = ProposalResolutionPayload {
            publication_id: proposal.publication_id.clone(),
            proposal_comment_id: 42,
            proposal_digest: proposal_digest.clone(),
            status: "accepted".to_string(),
            public_node_id: None,
        };
        let resolution_body = encode_proposal_resolution(&resolution).unwrap();
        assert_eq!(
            parse_proposal_resolution(&resolution_body)
                .unwrap()
                .proposal_comment_id,
            42
        );
        let mut edited = proposal;
        edited.prompt.push_str(" Edited.");
        assert_ne!(research_proposal_digest(&edited).unwrap(), proposal_digest);
    }

    #[test]
    fn parses_the_last_github_pagination_page() {
        assert_eq!(
            github_last_page(
                r#"<https://api.github.com/gists/abc/comments?per_page=100&page=2>; rel="next", <https://api.github.com/gists/abc/comments?per_page=100&page=6>; rel="last""#
            ),
            Some(6)
        );
        assert_eq!(github_last_page("not a link"), None);
    }

    #[test]
    fn canonical_json_sorts_object_keys() {
        let value = serde_json::json!({"z": [2, 1], "a": {"b": true, "a": null}});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"a":null,"b":true},"z":[2,1]}"#
        );
    }
}
