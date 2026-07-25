//! The saved prompt library: reusable composer messages stored as plain markdown
//! files, one file per prompt. The filesystem is the source of truth — files are
//! diffable, editable outside the app, and there is no index to migrate or fall
//! out of sync. The prompt's name is its filename stem; its content is the file
//! body.
//!
//! Prompts live in one of two scopes: `global` (`~/.qmux/prompts/`, visible from
//! every project) and `project`, keyed by the active pane's project directory and
//! stored centrally at `~/.qmux/projects/<basename>-<hash>/prompts/` — one flat
//! level per project path, so repos never grow a `.qmux` dir. The store dir name
//! combines the project's basename (readable) with a short SHA-256 of its
//! canonical path (collision-proof); `meta.json` alongside records the full path
//! since the hash is not reversible. Moving a prompt between scopes is a save
//! into the target directory followed by a delete from the source.

use crate::persistence;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

const PROMPTS_DIR: &str = "prompts";
const PROJECTS_DIR: &str = "projects";
const META_FILE: &str = "meta.json";
const PROMPT_EXTENSION: &str = "md";
/// Filenames land in menus and Finder; anything longer is a paragraph, not a name.
const MAX_NAME_CHARS: usize = 120;
/// Store dir names stay scannable in Finder: a readable basename prefix plus the
/// hash suffix, comfortably under filesystem name limits.
const MAX_BASENAME_CHARS: usize = 40;
/// 12 hex chars (48 bits) of the canonical-path SHA-256: far beyond collision
/// range for the number of projects one user has, while keeping names short.
const HASH_CHARS: usize = 12;

/// Serializes save/delete pairs (a rename's or move's write-then-remove) so two UI
/// surfaces can't interleave and leave both the old and new file behind.
static LIBRARY_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// Distinguishes concurrent writers' scratch files, mirroring persistence::TMP_SEQ.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptScope {
    Global,
    Project,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPrompt {
    pub name: String,
    pub content: String,
    pub modified_ms: u64,
    pub scope: PromptScope,
}

/// The whole library plus whether a project scope exists for the caller's
/// context: absent when no project directory was supplied (e.g. a pane with no
/// group dir), in which case the UI collapses to a single Global section.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptLibrary {
    pub prompts: Vec<SavedPrompt>,
    pub has_project_scope: bool,
}

/// Recorded next to each project's prompts so the hashed dir name can be mapped
/// back to the project it belongs to (by tooling, future GC, or a curious user).
#[derive(Serialize)]
struct ProjectMeta<'a> {
    path: &'a str,
}

fn qmux_home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(persistence::STATE_DIR))
        .ok_or_else(|| "HOME is not set; the prompt library is unavailable".to_string())
}

fn global_dir() -> Result<PathBuf, String> {
    Ok(qmux_home()?.join(PROMPTS_DIR))
}

/// A filesystem-safe, bounded prefix of the project's basename. Only used for
/// readability; uniqueness comes from the hash suffix.
fn sanitized_basename(path: &Path) -> String {
    let raw = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .skip_while(|&c| c == '.' || c == '-')
        .take(MAX_BASENAME_CHARS)
        .collect();
    if cleaned.is_empty() {
        "project".to_string()
    } else {
        cleaned
    }
}

/// One project's resolved central store: `~/.qmux/projects/<basename>-<hash>`.
/// Resolving canonicalizes and hashes the path, so callers resolve once and
/// reuse it rather than re-deriving per operation.
struct ProjectStore {
    dir: PathBuf,
    canonical: PathBuf,
}

impl ProjectStore {
    /// The path is canonicalized first so symlinked or differently-spelled
    /// routes to the same project share one store; a project that can't be
    /// canonicalized (deleted, permission) falls back to hashing the path as
    /// given. In practice qmux group dirs are already canonical (workspace.rs
    /// canonicalizes them at creation), so both branches agree.
    fn resolve(project: &Path) -> Result<Self, String> {
        let canonical = project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let hash: String = digest
            .iter()
            .take(HASH_CHARS / 2)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let dir = qmux_home()?
            .join(PROJECTS_DIR)
            .join(format!("{}-{hash}", sanitized_basename(&canonical)));
        Ok(Self { dir, canonical })
    }

    fn prompts_dir(&self) -> PathBuf {
        self.dir.join(PROMPTS_DIR)
    }

    /// Writes the store's meta.json if it doesn't exist yet, so a hashed dir is
    /// never left unexplained. Best-effort: failing to record the path must not
    /// block the prompt operation itself.
    fn ensure_meta(&self) {
        let meta_path = self.dir.join(META_FILE);
        if meta_path.exists() {
            return;
        }
        let meta = ProjectMeta {
            path: &self.canonical.to_string_lossy(),
        };
        if let Ok(raw) = serde_json::to_string_pretty(&meta) {
            let _ = fs::create_dir_all(&self.dir);
            let _ = fs::write(&meta_path, raw);
        }
    }
}

fn project_prompts_dir(project: &Path) -> Result<PathBuf, String> {
    Ok(ProjectStore::resolve(project)?.prompts_dir())
}

pub fn scope_dir(project: Option<&Path>, scope: PromptScope) -> Result<PathBuf, String> {
    match scope {
        PromptScope::Global => global_dir(),
        PromptScope::Project => match project {
            Some(project) => project_prompts_dir(project),
            None => Err("no project directory for project-scoped prompts".to_string()),
        },
    }
}

/// Resolves a scope's prompts dir and materializes it on disk (including the
/// project store's meta.json), for surfaces like reveal-in-Finder that hand the
/// directory to something external.
pub fn materialize_scope_dir(
    project: Option<&Path>,
    scope: PromptScope,
) -> Result<PathBuf, String> {
    let dir = match (scope, project) {
        (PromptScope::Global, _) => global_dir()?,
        (PromptScope::Project, Some(project)) => {
            let store = ProjectStore::resolve(project)?;
            store.ensure_meta();
            store.prompts_dir()
        }
        (PromptScope::Project, None) => {
            return Err("no project directory for project-scoped prompts".to_string());
        }
    };
    fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create prompts dir {}: {err}", dir.display()))?;
    Ok(dir)
}

/// Validates a prompt name and returns it trimmed. Names become filename stems,
/// so path separators, traversal dots, and control characters are rejected rather
/// than escaped — the stored name should read back exactly as typed.
fn validated_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("prompt name is empty".to_string());
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "prompt name is longer than {MAX_NAME_CHARS} characters"
        ));
    }
    if trimmed.starts_with('.') {
        return Err("prompt name can't start with a dot".to_string());
    }
    if trimmed
        .chars()
        .any(|c| c == '/' || c == '\\' || c == ':' || c.is_control())
    {
        return Err("prompt name can't contain /, \\, : or control characters".to_string());
    }
    Ok(trimmed.to_string())
}

fn prompt_path(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = validated_name(name)?;
    Ok(dir.join(format!("{name}.{PROMPT_EXTENSION}")))
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Lists every readable `.md` prompt in `dir`, tagged with `scope` and sorted by
/// name (case-insensitive). A missing directory is an empty scope, not an error,
/// but an unreadable one IS an error: rendering hidden prompts as an empty
/// section would invite the user to re-create (and silently overwrite) them.
/// Individually unreadable or non-UTF-8 files are still skipped so one damaged
/// file can't hide the rest of the library.
fn list_dir(dir: &Path, scope: PromptScope) -> Result<Vec<SavedPrompt>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "failed to read prompts dir {}: {err}",
                dir.display()
            ));
        }
    };

    let mut prompts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(PROMPT_EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        prompts.push(SavedPrompt {
            name: name.to_string(),
            content,
            modified_ms: modified_ms(&path),
            scope,
        });
    }
    prompts.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(prompts)
}

/// Lists the global scope plus, when a project directory is supplied, that
/// project's scope. Read failures propagate (see list_dir) rather than
/// masquerading as an empty library.
pub fn list(project: Option<&Path>) -> Result<PromptLibrary, String> {
    let mut prompts = list_dir(&global_dir()?, PromptScope::Global)?;
    let has_project_scope = project.is_some();
    if let Some(project) = project {
        prompts.extend(list_dir(
            &project_prompts_dir(project)?,
            PromptScope::Project,
        )?);
    }
    Ok(PromptLibrary {
        prompts,
        has_project_scope,
    })
}

/// The conflict a stale dialog hits: the prompt it was editing/moving/deleting
/// changed on disk since it was loaded, or a create would clobber a name another
/// surface just claimed. Callers refresh and retry.
const PROMPT_CONFLICT_MESSAGE: &str =
    "this prompt changed since it was loaded; refresh the library and try again";

/// Fails when `path`'s current mtime differs from what the caller last saw, so a
/// stale save/move/delete cannot silently overwrite or remove a prompt another
/// surface updated. `None` opts out (callers with no snapshot to compare). Runs
/// under LIBRARY_LOCK, so the check and the subsequent write don't interleave.
fn ensure_prompt_unchanged(path: &Path, expected_modified_ms: Option<u64>) -> Result<(), String> {
    let Some(expected) = expected_modified_ms else {
        return Ok(());
    };
    if !path.exists() || modified_ms(path) != expected {
        return Err(PROMPT_CONFLICT_MESSAGE.to_string());
    }
    Ok(())
}

/// Writes `content` under `name` in `dir`, atomically (temp + fsync + rename).
/// With `create_only`, refuses to overwrite an existing prompt of that name so
/// two surfaces deriving the same filename can't clobber each other (serialized
/// by LIBRARY_LOCK, so the existence check and the rename don't race in-process).
fn write_prompt(
    dir: &Path,
    name: &str,
    content: &str,
    create_only: bool,
) -> Result<PathBuf, String> {
    let path = prompt_path(dir, name)?;
    if create_only && path.exists() {
        return Err(PROMPT_CONFLICT_MESSAGE.to_string());
    }
    fs::create_dir_all(dir)
        .map_err(|err| format!("failed to create prompts dir {}: {err}", dir.display()))?;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!(
        "{PROMPT_EXTENSION}.{}.{seq}.tmp",
        std::process::id()
    ));
    persistence::write_synced(&tmp, content.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("failed to commit {}: {err}", path.display())
    })?;
    Ok(path)
}

fn remove_prompt_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("failed to delete {}: {err}", path.display())),
    }
}

/// Creates or overwrites the prompt `name` in `scope`. When `previous` names a
/// different (scope, name) location this is a rename and/or a move between scopes:
/// the new file is committed first, then the old one removed, so an interruption
/// can duplicate a prompt but never lose one.
///
/// Optimistic concurrency: a create (no `previous`) or a move (a `previous` that
/// differs from the target) refuses to clobber an existing prompt at the target;
/// an in-place update, and the removal of a move's source, require the file's
/// mtime to still equal `expected_modified_ms` (the value the caller loaded).
/// `expected_modified_ms` is `None` for a fresh create, which has nothing to
/// compare against.
pub fn save(
    project: Option<&Path>,
    scope: PromptScope,
    name: &str,
    content: &str,
    previous: Option<(PromptScope, &str)>,
    expected_modified_ms: Option<u64>,
) -> Result<SavedPrompt, String> {
    let _guard = LIBRARY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // Resolve the project store once: canonicalize + hash would otherwise run
    // for the write, the meta check, and again for a same-scope rename.
    let needs_project = scope == PromptScope::Project
        || previous.is_some_and(|(previous_scope, _)| previous_scope == PromptScope::Project);
    let store = match (needs_project, project) {
        (true, Some(project)) => Some(ProjectStore::resolve(project)?),
        (true, None) => {
            return Err("no project directory for project-scoped prompts".to_string());
        }
        (false, _) => None,
    };
    let dir_for = |wanted: PromptScope| -> Result<PathBuf, String> {
        match wanted {
            PromptScope::Global => global_dir(),
            // needs_project guarantees the store exists for any Project ask.
            PromptScope::Project => Ok(store.as_ref().unwrap().prompts_dir()),
        }
    };

    let dir = dir_for(scope)?;
    let target_path = prompt_path(&dir, name)?;
    let previous_path = match previous {
        Some((previous_scope, previous_name)) => {
            Some(prompt_path(&dir_for(previous_scope)?, previous_name)?)
        }
        None => None,
    };

    // An in-place update overwrites the same file (so require it unchanged); a
    // create or a move must not clobber the target (so write create-only), and a
    // move must also find its source unchanged before removing it.
    let is_in_place_update = previous_path.as_ref() == Some(&target_path);
    if is_in_place_update {
        ensure_prompt_unchanged(&target_path, expected_modified_ms)?;
    } else if let Some(previous_path) = &previous_path {
        ensure_prompt_unchanged(previous_path, expected_modified_ms)?;
    }

    let path = write_prompt(&dir, name, content, !is_in_place_update)?;
    if scope == PromptScope::Project
        && let Some(store) = &store
    {
        store.ensure_meta();
    }

    if let Some(previous_path) = previous_path {
        if previous_path != path {
            remove_prompt_file(&previous_path)
                .map_err(|err| format!("saved, but couldn't remove the old prompt: {err}"))?;
        }
    }

    Ok(SavedPrompt {
        name: validated_name(name)?,
        content: content.to_string(),
        modified_ms: modified_ms(&path),
        scope,
    })
}

/// Removes the prompt `name` from `scope`. Deleting a prompt that is already gone
/// succeeds: the caller's goal (the file no longer exists) is met either way. But
/// when the file still exists and `expected_modified_ms` no longer matches, the
/// prompt was updated by another surface since this dialog loaded it, so the
/// delete is refused rather than discarding that newer content.
pub fn delete(
    project: Option<&Path>,
    scope: PromptScope,
    name: &str,
    expected_modified_ms: Option<u64>,
) -> Result<(), String> {
    let _guard = LIBRARY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = scope_dir(project, scope)?;
    let path = prompt_path(&dir, name)?;
    if !path.exists() {
        return Ok(());
    }
    if let Some(expected) = expected_modified_ms
        && modified_ms(&path) != expected
    {
        return Err(PROMPT_CONFLICT_MESSAGE.to_string());
    }
    remove_prompt_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("qmux-prompts-{label}-{nanos}-{seq}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_dir_lists_nothing() {
        let dir = temp_dir("empty").join("missing");
        assert!(list_dir(&dir, PromptScope::Project).unwrap().is_empty());
    }

    #[test]
    fn store_dir_is_one_level_with_basename_and_hash() {
        let project = temp_dir("store");
        let store = ProjectStore::resolve(&project).unwrap();

        let name = store.dir.file_name().unwrap().to_str().unwrap();
        let stem = store.dir.parent().unwrap();
        assert_eq!(stem.file_name().unwrap(), PROJECTS_DIR);
        // <sanitized-basename>-<12 hex chars>
        let (prefix, hash) = name.rsplit_once('-').unwrap();
        assert!(prefix.starts_with("qmux-prompts-store"));
        assert_eq!(hash.len(), HASH_CHARS);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(store.canonical.is_absolute());

        // Deterministic: the same project maps to the same store.
        assert_eq!(store.dir, ProjectStore::resolve(&project).unwrap().dir);
    }

    #[test]
    fn store_dir_distinguishes_same_basename_projects() {
        let a = temp_dir("same").join("app");
        let b = temp_dir("same").join("app");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_ne!(
            ProjectStore::resolve(&a).unwrap().dir,
            ProjectStore::resolve(&b).unwrap().dir
        );
    }

    #[test]
    fn store_dir_sanitizes_hostile_basenames() {
        let base = temp_dir("hostile");
        let project = base.join("my répo/**");
        // Non-existent (can't canonicalize) — falls back to the literal path.
        let store = ProjectStore::resolve(&project).unwrap();
        let name = store.dir.file_name().unwrap().to_str().unwrap();
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
            "unsafe store name {name:?}"
        );
    }

    #[test]
    fn project_save_writes_meta_and_round_trips() {
        let project = temp_dir("meta");
        save(
            Some(&project),
            PromptScope::Project,
            "Review checklist",
            "Review {target} for bugs.",
            None,
            None,
        )
        .unwrap();

        let store = ProjectStore::resolve(&project).unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.dir.join(META_FILE)).unwrap()).unwrap();
        assert_eq!(meta["path"], store.canonical.to_string_lossy().as_ref());

        let dir = store.prompts_dir();
        let prompts = list_dir(&dir, PromptScope::Project).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "Review checklist");
        assert_eq!(prompts[0].content, "Review {target} for bugs.");

        delete(
            Some(&project),
            PromptScope::Project,
            "Review checklist",
            None,
        )
        .unwrap();
        assert!(list_dir(&dir, PromptScope::Project).unwrap().is_empty());
        // Deleting an already-missing prompt is a no-op, not an error.
        delete(
            Some(&project),
            PromptScope::Project,
            "Review checklist",
            None,
        )
        .unwrap();

        // Clean up the real ~/.qmux/projects entry this test created.
        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn materialize_creates_dir_and_meta() {
        let project = temp_dir("materialize");
        let dir = materialize_scope_dir(Some(&project), PromptScope::Project).unwrap();
        assert!(dir.is_dir());

        let store = ProjectStore::resolve(&project).unwrap();
        assert!(store.dir.join(META_FILE).exists());

        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn rename_within_scope_moves_the_file() {
        let project = temp_dir("rename");
        save(
            Some(&project),
            PromptScope::Project,
            "old name",
            "body",
            None,
            None,
        )
        .unwrap();
        save(
            Some(&project),
            PromptScope::Project,
            "new name",
            "body v2",
            Some((PromptScope::Project, "old name")),
            None,
        )
        .unwrap();

        let store = ProjectStore::resolve(&project).unwrap();
        let prompts = list_dir(&store.prompts_dir(), PromptScope::Project).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "new name");
        assert_eq!(prompts[0].content, "body v2");

        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn project_scope_without_project_dir_errors() {
        assert!(save(None, PromptScope::Project, "note", "body", None, None).is_err());
        assert!(delete(None, PromptScope::Project, "note", None).is_err());
        assert!(scope_dir(None, PromptScope::Project).is_err());
        assert!(materialize_scope_dir(None, PromptScope::Project).is_err());
        // A global save whose `previous` names the project scope needs one too.
        assert!(
            save(
                None,
                PromptScope::Global,
                "note",
                "body",
                Some((PromptScope::Project, "note")),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn list_without_project_has_no_project_scope() {
        let library = list(None).unwrap();
        assert!(!library.has_project_scope);
        assert!(
            library
                .prompts
                .iter()
                .all(|p| p.scope == PromptScope::Global)
        );
    }

    #[test]
    fn rejects_unsafe_names() {
        let project = temp_dir("names");
        for bad in [
            "",
            "  ",
            "../escape",
            "a/b",
            "a\\b",
            ".hidden",
            "a:b",
            "a\nb",
        ] {
            assert!(
                save(
                    Some(&project),
                    PromptScope::Project,
                    bad,
                    "body",
                    None,
                    None
                )
                .is_err(),
                "accepted {bad:?}"
            );
        }
        let dir = project_prompts_dir(&project).unwrap();
        assert!(list_dir(&dir, PromptScope::Project).unwrap().is_empty());
    }

    #[test]
    fn skips_non_markdown_files() {
        let project = temp_dir("filter");
        save(
            Some(&project),
            PromptScope::Project,
            "keep",
            "body",
            None,
            None,
        )
        .unwrap();
        let store = ProjectStore::resolve(&project).unwrap();
        fs::write(store.prompts_dir().join("notes.txt"), "ignored").unwrap();
        let prompts = list_dir(&store.prompts_dir(), PromptScope::Project).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "keep");

        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn scope_serialization_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&PromptScope::Global).unwrap(),
            "\"global\""
        );
        assert_eq!(
            serde_json::from_str::<PromptScope>("\"project\"").unwrap(),
            PromptScope::Project
        );
    }

    #[test]
    fn create_only_save_refuses_to_clobber() {
        let project = temp_dir("create-only");
        save(
            Some(&project),
            PromptScope::Project,
            "dup",
            "first",
            None,
            None,
        )
        .unwrap();
        // A second create (no `previous`) at the same name must not overwrite —
        // this is how two panes deriving the same filename are resolved.
        assert!(
            save(
                Some(&project),
                PromptScope::Project,
                "dup",
                "second",
                None,
                None
            )
            .is_err()
        );
        let store = ProjectStore::resolve(&project).unwrap();
        let prompts = list_dir(&store.prompts_dir(), PromptScope::Project).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].content, "first");
        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn stale_update_is_rejected_and_current_survives() {
        let project = temp_dir("stale-update");
        let saved = save(
            Some(&project),
            PromptScope::Project,
            "note",
            "v1",
            None,
            None,
        )
        .unwrap();
        let previous = Some((PromptScope::Project, "note"));
        // A stale in-place update (mismatched expected mtime) is refused.
        assert!(
            save(
                Some(&project),
                PromptScope::Project,
                "note",
                "v2",
                previous,
                Some(saved.modified_ms.wrapping_add(1)),
            )
            .is_err()
        );
        // The matching expected mtime still succeeds.
        save(
            Some(&project),
            PromptScope::Project,
            "note",
            "v2",
            previous,
            Some(saved.modified_ms),
        )
        .unwrap();
        let store = ProjectStore::resolve(&project).unwrap();
        let prompts = list_dir(&store.prompts_dir(), PromptScope::Project).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].content, "v2");
        let _ = fs::remove_dir_all(store.dir);
    }

    #[test]
    fn stale_delete_is_rejected_and_matching_delete_succeeds() {
        let project = temp_dir("stale-delete");
        let saved = save(
            Some(&project),
            PromptScope::Project,
            "keep",
            "body",
            None,
            None,
        )
        .unwrap();
        let store = ProjectStore::resolve(&project).unwrap();
        // A stale delete (mismatched expected mtime) leaves the prompt in place.
        assert!(
            delete(
                Some(&project),
                PromptScope::Project,
                "keep",
                Some(saved.modified_ms.wrapping_add(1)),
            )
            .is_err()
        );
        assert_eq!(
            list_dir(&store.prompts_dir(), PromptScope::Project)
                .unwrap()
                .len(),
            1
        );
        // The matching mtime deletes; a repeat on the now-missing file is a no-op.
        delete(
            Some(&project),
            PromptScope::Project,
            "keep",
            Some(saved.modified_ms),
        )
        .unwrap();
        assert!(
            list_dir(&store.prompts_dir(), PromptScope::Project)
                .unwrap()
                .is_empty()
        );
        delete(
            Some(&project),
            PromptScope::Project,
            "keep",
            Some(saved.modified_ms),
        )
        .unwrap();
        let _ = fs::remove_dir_all(store.dir);
    }
}
