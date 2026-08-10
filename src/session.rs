use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
    pub tmux_window: String,
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

pub fn load_sessions(repo_root: &Path) -> Result<Vec<Session>> {
    let path = sessions_path(repo_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading sessions file {}", path.display()))?;
    let file: SessionFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing sessions file {}", path.display()))?;
    Ok(file.sessions)
}

pub fn save_sessions(repo_root: &Path, sessions: &[Session]) -> Result<()> {
    let path = sessions_path(repo_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = SessionFile {
        sessions: sessions.to_vec(),
    };
    let text = serde_json::to_string_pretty(&file)?;
    std::fs::write(&path, text)?;
    Ok(())
}

pub fn find_session<'a>(sessions: &'a [Session], slug: &str) -> Option<&'a Session> {
    sessions.iter().find(|s| s.slug == slug)
}

pub fn add_session(repo_root: &Path, session: Session) -> Result<()> {
    let mut sessions = load_sessions(repo_root)?;
    if find_session(&sessions, &session.slug).is_some() {
        return Err(AmError::SlugAlreadyExists(session.slug.clone()).into());
    }
    sessions.push(session);
    save_sessions(repo_root, &sessions)
}

pub fn remove_session(repo_root: &Path, slug: &str) -> Result<()> {
    let mut sessions = load_sessions(repo_root)?;
    let before = sessions.len();
    sessions.retain(|s| s.slug != slug);
    if sessions.len() == before {
        return Err(AmError::SlugNotFound(slug.to_string()).into());
    }
    save_sessions(repo_root, &sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_session(slug: &str) -> Session {
        Session {
            slug: slug.to_string(),
            created_at: Utc::now(),
            auto: false,
            vcs: VcsMetadata {
                branch: format!("am/{slug}"),
                worktree_path: PathBuf::from(format!(".am/worktrees/{slug}")),
            },
            tmux: TmuxMetadata {
                tmux_window: format!("am-{slug}"),
                agent_pane: format!("am-{slug}.1"),
                shell_pane: format!("am-{slug}.0"),
                original_window_name: None,
                original_shell_dir: None,
            },
            container: None,
        }
    }

    #[test]
    fn missing_sessions_file_returns_empty_list() {
        let tmp = TempDir::new().unwrap();
        let sessions = load_sessions(tmp.path()).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn add_and_find_session() {
        let tmp = TempDir::new().unwrap();
        let session = make_session("feat");
        add_session(tmp.path(), session.clone()).unwrap();

        let sessions = load_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        let found = find_session(&sessions, "feat").unwrap();
        assert_eq!(found.slug, "feat");
        assert_eq!(found.vcs.branch, "am/feat");
    }

    #[test]
    fn find_session_returns_none_for_missing_slug() {
        let sessions = vec![make_session("feat")];
        assert!(find_session(&sessions, "missing").is_none());
    }

    #[test]
    fn add_duplicate_slug_errors() {
        let tmp = TempDir::new().unwrap();
        add_session(tmp.path(), make_session("feat")).unwrap();
        let err = add_session(tmp.path(), make_session("feat")).unwrap_err();
        assert!(err.to_string().contains("feat"));
    }

    #[test]
    fn remove_session_success() {
        let tmp = TempDir::new().unwrap();
        add_session(tmp.path(), make_session("feat")).unwrap();
        add_session(tmp.path(), make_session("bugfix")).unwrap();

        remove_session(tmp.path(), "feat").unwrap();

        let sessions = load_sessions(tmp.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].slug, "bugfix");
    }

    #[test]
    fn remove_nonexistent_slug_errors() {
        let tmp = TempDir::new().unwrap();
        let err = remove_session(tmp.path(), "ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn sessions_roundtrip_json() {
        let tmp = TempDir::new().unwrap();
        let mut s = make_session("feat");
        let mut container =
            SessionContainer::image_mode("podman".to_string(), "myimage:latest".to_string());
        container.container_id = Some("abc123".to_string());
        s.container = Some(container);
        add_session(tmp.path(), s).unwrap();

        let loaded = load_sessions(tmp.path()).unwrap();
        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.runtime, "podman");
        assert_eq!(c.container_id.as_deref(), Some("abc123"));
        assert_eq!(c.mode, ContainerMode::Image);
    }

    #[test]
    fn devcontainer_session_roundtrips_json() {
        let tmp = TempDir::new().unwrap();
        let mut s = make_session("feat");
        s.container = Some(SessionContainer {
            runtime: "podman".to_string(),
            image: "am-dc-abc123".to_string(),
            container_id: None,
            mode: ContainerMode::Devcontainer,
            config_path: Some(PathBuf::from(".devcontainer/devcontainer.json")),
            config_hash: Some("abc123".to_string()),
            remote_user: Some("vscode".to_string()),
            lifecycle_done: vec!["postCreateCommand".to_string()],
        });
        add_session(tmp.path(), s).unwrap();

        let loaded = load_sessions(tmp.path()).unwrap();
        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.mode, ContainerMode::Devcontainer);
        assert_eq!(c.config_hash.as_deref(), Some("abc123"));
        assert_eq!(c.remote_user.as_deref(), Some("vscode"));
        assert_eq!(c.lifecycle_done, vec!["postCreateCommand".to_string()]);
    }

    #[test]
    fn records_written_before_devcontainer_support_still_load() {
        // Sessions on disk predate every field added for devcontainer mode; a user
        // upgrading am must not have to destroy their running sessions.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        std::fs::write(
            tmp.path().join(".am").join("sessions.json"),
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

        let loaded = load_sessions(tmp.path()).unwrap();

        let c = loaded[0].container.as_ref().unwrap();
        assert_eq!(c.mode, ContainerMode::Image);
        assert!(c.config_hash.is_none());
        assert!(c.lifecycle_done.is_empty());
    }
}
