use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::color;
use crate::config;
use crate::error::AmError;

/// Where a session's image came from. Recorded so `am list` can distinguish the two and
/// so a rebuild can tell whether the environment is still current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContainerMode {
    #[default]
    Image,
    Devcontainer,
}

impl std::fmt::Display for ContainerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerMode::Image => write!(f, "image"),
            ContainerMode::Devcontainer => write!(f, "devcontainer"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContainer {
    pub runtime: String,
    pub image: String,
    /// Runtime name used to address this container. Unlike a tmux title, this
    /// is globally unique across repositories.
    #[serde(default)]
    pub container_name: Option<String>,
    pub container_id: Option<String>,
    /// Defaults to `Image` so records written before devcontainer support still load.
    #[serde(default)]
    pub mode: ContainerMode,
    /// The devcontainer config this session was built from, if any.
    #[serde(default)]
    pub config_path: Option<PathBuf>,
    /// Hash of that config, for detecting a stale environment.
    #[serde(default)]
    pub config_hash: Option<String>,
    /// `remoteUser`/`containerUser` the image runs as.
    #[serde(default)]
    pub remote_user: Option<String>,
    /// Which create-time lifecycle hooks have already run, so they run exactly once.
    #[serde(default)]
    pub lifecycle_done: Vec<String>,
}

impl SessionContainer {
    /// A record for a plain `am`-resolved image, with no devcontainer involvement.
    pub fn image_mode(runtime: String, image: String) -> Self {
        Self {
            runtime,
            image,
            container_name: None,
            container_id: None,
            mode: ContainerMode::Image,
            config_path: None,
            config_hash: None,
            remote_user: None,
            lifecycle_done: Vec::new(),
        }
    }
}

/// VCS-specific metadata for a session (git branch and worktree path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcsMetadata {
    pub branch: String,
    pub worktree_path: PathBuf,
}

/// Tmux-specific metadata for a session (window and pane names, original state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxMetadata {
    /// Human-facing tmux window title. Titles are allowed to collide.
    pub tmux_window: String,
    /// Stable tmux target (for example `@42`) used for programmatic actions.
    #[serde(default)]
    pub tmux_window_id: Option<String>,
    pub agent_pane: String,
    pub shell_pane: String,
    /// The window name before `am start` renamed it (new-style sessions only).
    /// `None` for old-style sessions that owned a dedicated window.
    #[serde(default)]
    pub original_window_name: Option<String>,
    /// The shell pane's working directory at session creation time.
    /// Used to restore the directory when the session is destroyed.
    #[serde(default)]
    pub original_shell_dir: Option<PathBuf>,
}

/// A session represents an agent's isolated environment with VCS, Tmux, and optional container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub slug: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub auto: bool,
    /// Absolute path of the repository this session belongs to.
    /// Populated by migration when reading old per-repo session files.
    #[serde(default)]
    pub repo_root: PathBuf,
    /// VCS-related metadata (branch, worktree path).
    #[serde(flatten)]
    pub vcs: VcsMetadata,
    /// Tmux-related metadata (window, pane names, original state).
    #[serde(flatten)]
    pub tmux: TmuxMetadata,
    pub container: Option<SessionContainer>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionFile {
    sessions: Vec<Session>,
}

fn sessions_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".am").join("sessions.json")
}

pub fn find_session<'a>(sessions: &'a [Session], slug: &str) -> Option<&'a Session> {
    sessions.iter().find(|s| s.slug == slug)
}

// ── Global session store ──────────────────────────────────────────────────────

/// Resolve the global sessions file path.
/// Returns `None` only if neither `XDG_STATE_HOME` nor `HOME` is set.
pub fn global_sessions_path() -> Option<PathBuf> {
    config::global_state_dir().map(|d| d.join("sessions.json"))
}

/// Acquire an exclusive interprocess lock for the global sessions file.
/// The returned `File` handle acts as the lock guard — dropping it releases the lock.
fn lock_global_sessions() -> Result<File> {
    let path = global_sessions_path().ok_or(AmError::GlobalStateDirNotFound)?;
    let lock_path = path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_file = File::create(&lock_path)
        .with_context(|| format!("creating lock file {}", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(lock_file)
}

/// Load all sessions from the global store.
/// Returns an empty `Vec` if the file does not exist.
pub fn load_all_sessions() -> Result<Vec<Session>> {
    let path = global_sessions_path().ok_or(AmError::GlobalStateDirNotFound)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading global sessions file {}", path.display()))?;
    let file: SessionFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing global sessions file {}", path.display()))?;
    Ok(file.sessions)
}

/// Load sessions belonging to a specific repo (filters by `repo_root`).
/// Returns an empty `Vec` if the global file does not exist yet.
pub fn load_sessions_for_repo(repo_root: &Path) -> Result<Vec<Session>> {
    Ok(load_all_sessions()?
        .into_iter()
        .filter(|s| s.repo_root == repo_root)
        .collect())
}

fn save_global_sessions(sessions: &[Session]) -> Result<()> {
    let path = global_sessions_path().ok_or(AmError::GlobalStateDirNotFound)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = SessionFile {
        sessions: sessions.to_vec(),
    };
    let text = serde_json::to_string_pretty(&file)?;

    // Write to a temporary file in the same directory, then rename into place.
    // rename() is atomic on the same filesystem, so readers never see a
    // partially-written sessions.json.
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, text)
        .with_context(|| format!("writing temporary sessions file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Add a session to the global store.
/// Errors with `SlugAlreadyExists` if `(repo_root, slug)` already exists.
pub fn add_session_global(session: Session) -> Result<()> {
    let _lock = lock_global_sessions()?;
    let mut sessions = load_all_sessions()?;
    if sessions
        .iter()
        .any(|s| s.repo_root == session.repo_root && s.slug == session.slug)
    {
        return Err(AmError::SlugAlreadyExists(session.slug.clone()).into());
    }
    sessions.push(session);
    save_global_sessions(&sessions)
}

/// Remove a session from the global store by `(repo_root, slug)`.
/// Errors with `SlugNotFound` if not found.
pub fn remove_session_global(repo_root: &Path, slug: &str) -> Result<()> {
    let _lock = lock_global_sessions()?;
    let mut sessions = load_all_sessions()?;
    let before = sessions.len();
    sessions.retain(|s| !(s.repo_root == repo_root && s.slug == slug));
    if sessions.len() == before {
        return Err(AmError::SlugNotFound(slug.to_string()).into());
    }
    save_global_sessions(&sessions)
}

/// Replace an existing session after mutable runtime metadata changes.
pub fn update_session_global(session: Session) -> Result<()> {
    let _lock = lock_global_sessions()?;
    let mut sessions = load_all_sessions()?;
    let existing = sessions
        .iter_mut()
        .find(|s| s.repo_root == session.repo_root && s.slug == session.slug)
        .ok_or_else(|| AmError::SlugNotFound(session.slug.clone()))?;
    *existing = session;
    save_global_sessions(&sessions)
}

/// Migrate sessions from an old per-repo file into the global store.
///
/// Reads `<repo_root>/.am/sessions.json`, sets `repo_root` on each record,
/// merges into the global store (skipping duplicates), then deletes the old file.
///
/// Returns the number of records migrated (0 if none, or if the file did not exist).
/// On parse error, prints a warning and returns 0 without deleting the file.
/// Idempotent: re-running after the old file is deleted is a no-op.
pub fn migrate_sessions(repo_root: &Path) -> Result<usize> {
    let old_path = sessions_path(repo_root);
    if !old_path.exists() {
        return Ok(0);
    }

    let text = match std::fs::read_to_string(&old_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "{} could not read .am/sessions.json for migration: {e} — leaving file in place",
                color::warning_prefix(color::enabled(color::Stream::Stderr))
            );
            return Ok(0);
        }
    };

    let old_file: SessionFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{} could not parse .am/sessions.json for migration: {e} — leaving file in place",
                color::warning_prefix(color::enabled(color::Stream::Stderr))
            );
            return Ok(0);
        }
    };

    if old_file.sessions.is_empty() {
        // Nothing to migrate; still delete the now-empty old file.
        let _ = std::fs::remove_file(&old_path);
        return Ok(0);
    }

    // Lock and load the global store; on error, propagate so we don't delete the old file.
    let _lock = lock_global_sessions()?;
    let mut global = load_all_sessions()?;

    let mut migrated = 0usize;
    for mut session in old_file.sessions {
        session.repo_root = repo_root.to_path_buf();
        // Skip if (repo_root, slug) already exists in the global store.
        let already_exists = global
            .iter()
            .any(|s| s.repo_root == session.repo_root && s.slug == session.slug);
        if !already_exists {
            global.push(session);
            migrated += 1;
        }
    }

    // Write global store before deleting old file — leave old file intact on write failure.
    save_global_sessions(&global)?;

    // Delete the old file now that the global write succeeded.
    let _ = std::fs::remove_file(&old_path);

    if migrated > 0 {
        eprintln!("Migrated {migrated} session(s) from .am/sessions.json to global store.");
    }

    Ok(migrated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Mutex to serialise all tests that mutate process-global env vars.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that saves the current value of env vars on construction and
    /// restores them on drop.
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn save(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn make_session(slug: &str) -> Session {
        Session {
            slug: slug.to_string(),
            created_at: Utc::now(),
            auto: false,
            repo_root: PathBuf::new(),
            vcs: VcsMetadata {
                branch: format!("am/{slug}"),
                worktree_path: PathBuf::from(format!(".am/worktrees/{slug}")),
            },
            tmux: TmuxMetadata {
                tmux_window: format!("am-{slug}"),
                tmux_window_id: None,
                agent_pane: format!("am-{slug}.1"),
                shell_pane: format!("am-{slug}.0"),
                original_window_name: None,
                original_shell_dir: None,
            },
            container: None,
        }
    }

    fn make_session_for_repo(slug: &str, repo_root: &Path) -> Session {
        let mut s = make_session(slug);
        s.repo_root = repo_root.to_path_buf();
        s
    }

    // ── find_session ────────────────────────────────────────────────────────────

    #[test]
    fn find_session_returns_none_for_missing_slug() {
        let sessions = vec![make_session("feat")];
        assert!(find_session(&sessions, "missing").is_none());
    }

    #[test]
    fn find_session_returns_match() {
        let sessions = vec![make_session("feat"), make_session("bugfix")];
        let found = find_session(&sessions, "feat").unwrap();
        assert_eq!(found.slug, "feat");
        assert_eq!(found.vcs.branch, "am/feat");
    }

    // ── JSON roundtrip via global store ──────────────────────────────────────

    #[test]
    fn sessions_roundtrip_json() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        let mut s = make_session_for_repo("feat", &repo);
        let mut container =
            SessionContainer::image_mode("podman".to_string(), "myimage:latest".to_string());
        container.container_id = Some("abc123".to_string());
        s.container = Some(container);
        add_session_global(s).unwrap();

        let loaded = load_all_sessions().unwrap();
        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.runtime, "podman");
        assert_eq!(c.container_id.as_deref(), Some("abc123"));
        assert_eq!(c.mode, ContainerMode::Image);
    }

    #[test]
    fn devcontainer_session_roundtrips_json() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        let mut s = make_session_for_repo("feat", &repo);
        s.container = Some(SessionContainer {
            runtime: "podman".to_string(),
            image: "am-dc-abc123".to_string(),
            container_name: Some("am-feat-deadbe".to_string()),
            container_id: None,
            mode: ContainerMode::Devcontainer,
            config_path: Some(PathBuf::from(".devcontainer/devcontainer.json")),
            config_hash: Some("abc123".to_string()),
            remote_user: Some("vscode".to_string()),
            lifecycle_done: vec!["postCreateCommand".to_string()],
        });
        add_session_global(s).unwrap();

        let loaded = load_all_sessions().unwrap();
        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.mode, ContainerMode::Devcontainer);
        assert_eq!(c.config_hash.as_deref(), Some("abc123"));
        assert_eq!(c.remote_user.as_deref(), Some("vscode"));
        assert_eq!(c.lifecycle_done, vec!["postCreateCommand".to_string()]);
    }

    #[test]
    fn legacy_records_without_repo_root_migrate_correctly() {
        // Sessions on disk predate the repo_root field and devcontainer fields;
        // migration must handle missing fields gracefully.
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".am")).unwrap();
        std::fs::write(
            repo.join(".am").join("sessions.json"),
            r#"{"sessions":[{
                "slug":"legacy",
                "created_at":"2026-01-01T00:00:00Z",
                "branch":"am/legacy",
                "worktree_path":".am/worktrees/legacy",
                "tmux_window":"am-legacy",
                "agent_pane":"am-legacy.1",
                "shell_pane":"am-legacy.0",
                "container":{"runtime":"podman","image":"old:latest","container_id":null}
            }]}"#,
        )
        .unwrap();

        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 1);

        let loaded = load_all_sessions().unwrap();
        assert_eq!(loaded[0].repo_root, repo);
        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.mode, ContainerMode::Image);
        assert!(c.config_hash.is_none());
        assert!(c.lifecycle_done.is_empty());
    }

    // ── Global session path ───────────────────────────────────────────────────

    #[test]
    fn global_sessions_path_uses_xdg_state_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());
        std::env::remove_var("HOME");

        let path = global_sessions_path();
        assert_eq!(path, Some(tmp.path().join("am").join("sessions.json")));
    }

    #[test]
    fn global_sessions_path_falls_back_to_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", tmp.path());

        let path = global_sessions_path();
        assert_eq!(
            path,
            Some(
                tmp.path()
                    .join(".local")
                    .join("state")
                    .join("am")
                    .join("sessions.json")
            )
        );
    }

    #[test]
    fn global_sessions_path_returns_none_without_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("HOME");

        assert_eq!(global_sessions_path(), None);
    }

    // ── add_session_global ────────────────────────────────────────────────────

    #[test]
    fn add_session_global_enforces_repo_slug_uniqueness() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        let s = make_session_for_repo("feat", &repo);
        add_session_global(s.clone()).unwrap();

        let err = add_session_global(s).unwrap_err();
        assert!(err.to_string().contains("feat"));
    }

    #[test]
    fn add_session_global_allows_same_slug_in_different_repos() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");

        add_session_global(make_session_for_repo("feat", &repo_a)).unwrap();
        add_session_global(make_session_for_repo("feat", &repo_b)).unwrap();

        let all = load_all_sessions().unwrap();
        assert_eq!(all.len(), 2);
    }

    // ── load_sessions_for_repo ────────────────────────────────────────────────

    #[test]
    fn load_sessions_for_repo_filters_by_repo_root() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");

        add_session_global(make_session_for_repo("feat", &repo_a)).unwrap();
        add_session_global(make_session_for_repo("bugfix", &repo_b)).unwrap();
        add_session_global(make_session_for_repo("other", &repo_a)).unwrap();

        let for_a = load_sessions_for_repo(&repo_a).unwrap();
        assert_eq!(for_a.len(), 2);
        assert!(for_a.iter().all(|s| s.repo_root == repo_a));

        let for_b = load_sessions_for_repo(&repo_b).unwrap();
        assert_eq!(for_b.len(), 1);
        assert_eq!(for_b[0].slug, "bugfix");
    }

    #[test]
    fn load_sessions_for_repo_returns_empty_when_no_global_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("nonexistent-repo");
        let sessions = load_sessions_for_repo(&repo).unwrap();
        assert!(sessions.is_empty());
    }

    // ── remove_session_global ─────────────────────────────────────────────────

    #[test]
    fn remove_session_global_removes_correct_record() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        add_session_global(make_session_for_repo("feat", &repo)).unwrap();
        add_session_global(make_session_for_repo("bugfix", &repo)).unwrap();

        remove_session_global(&repo, "feat").unwrap();

        let remaining = load_all_sessions().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].slug, "bugfix");
    }

    #[test]
    fn remove_session_global_errors_when_not_found() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        let err = remove_session_global(&repo, "ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    // ── migrate_sessions ──────────────────────────────────────────────────────

    fn write_old_sessions_file(repo_root: &Path, sessions_json: &str) {
        let am_dir = repo_root.join(".am");
        std::fs::create_dir_all(&am_dir).unwrap();
        std::fs::write(am_dir.join("sessions.json"), sessions_json).unwrap();
    }

    #[test]
    fn migrate_sessions_moves_records_and_deletes_old_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        write_old_sessions_file(
            &repo,
            r#"{"sessions":[{
                "slug":"feat",
                "created_at":"2026-01-01T00:00:00Z",
                "branch":"am/feat",
                "worktree_path":".am/worktrees/feat",
                "tmux_window":"am-feat",
                "agent_pane":"am-feat.1",
                "shell_pane":"am-feat.0"
            }]}"#,
        );

        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 1);

        // Old file should be deleted.
        assert!(!repo.join(".am").join("sessions.json").exists());

        // Record should be in the global store with repo_root set.
        let all = load_all_sessions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].slug, "feat");
        assert_eq!(all[0].repo_root, repo);
    }

    #[test]
    fn migrate_sessions_is_idempotent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        write_old_sessions_file(
            &repo,
            r#"{"sessions":[{
                "slug":"feat",
                "created_at":"2026-01-01T00:00:00Z",
                "branch":"am/feat",
                "worktree_path":".am/worktrees/feat",
                "tmux_window":"am-feat",
                "agent_pane":"am-feat.1",
                "shell_pane":"am-feat.0"
            }]}"#,
        );

        migrate_sessions(&repo).unwrap();
        // Second call — old file is gone, should return 0 without error.
        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 0);

        let all = load_all_sessions().unwrap();
        assert_eq!(all.len(), 1, "no duplicates after idempotent migration");
    }

    #[test]
    fn migrate_sessions_skips_duplicates() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");

        // Pre-seed the global store with the same session.
        add_session_global(make_session_for_repo("feat", &repo)).unwrap();

        // Write old file with same slug.
        write_old_sessions_file(
            &repo,
            r#"{"sessions":[{
                "slug":"feat",
                "created_at":"2026-01-01T00:00:00Z",
                "branch":"am/feat",
                "worktree_path":".am/worktrees/feat",
                "tmux_window":"am-feat",
                "agent_pane":"am-feat.1",
                "shell_pane":"am-feat.0"
            }]}"#,
        );

        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 0, "duplicate should be skipped");

        // Global store must not have duplicates.
        let all = load_all_sessions().unwrap();
        assert_eq!(all.len(), 1);
        // Old file should be deleted even though 0 records were migrated
        // (all were duplicates).
        assert!(!repo.join(".am").join("sessions.json").exists());
    }

    #[test]
    fn migrate_sessions_handles_malformed_old_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        write_old_sessions_file(&repo, "this is not json {{{");

        // Should return 0 and leave the file in place.
        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 0);
        assert!(
            repo.join(".am").join("sessions.json").exists(),
            "malformed file should be left in place"
        );
    }

    #[test]
    fn migrate_sessions_handles_zero_records_deletes_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_STATE_HOME", tmp.path());

        let repo = tmp.path().join("repo");
        write_old_sessions_file(&repo, r#"{"sessions":[]}"#);

        let count = migrate_sessions(&repo).unwrap();
        assert_eq!(count, 0);
        // Empty old file should be deleted silently.
        assert!(!repo.join(".am").join("sessions.json").exists());
    }
}
