use thiserror::Error;

#[derive(Debug, Error)]
pub enum AmError {
    #[error("not in a git or jj repository")]
    NotInRepo,

    #[error("not inside a tmux session (run inside tmux or use 'tmux new-session' first)")]
    NotInTmux,

    #[error("slug '{0}' already exists — run 'am attach {0}' to resume it, or 'am destroy {0}' first")]
    SlugAlreadyExists(String),

    #[error("slug '{0}' not found — run 'am list' to see active sessions")]
    SlugNotFound(String),

    #[error("worktree error: {0}")]
    WorktreeError(String),

    #[error("tmux error: {0}")]
    TmuxError(String),

    #[error("no container runtime found — install Podman or Docker. See Podman: https://podman.io/getting-started/installation or Docker: https://docs.docker.com/get-docker/")]
    ContainerRuntimeNotFound,

    #[error("requested container runtime '{0}' not found — install it or change .am/config.toml (container.runtime)")]
    RequestedContainerRuntimeNotFound(String),

    #[error("no container image configured — set an agent with `--agent` or `defaults.agent` in config (image is selected automatically), or set `container.image` for a custom image")]
    ContainerImageNotConfigured,

    #[error(
        "--auto requires container isolation; --no-container and --auto cannot be used together"
    )]
    AutoRequiresContainer,

    #[error(
        "--auto requires an agent; set one with --agent or configure `agent` in .am/config.toml"
    )]
    AutoRequiresAgent,

    #[error("container error: {0}")]
    ContainerError(String),


    #[error("devcontainer build failed: {0}")]
    DevcontainerBuildFailed(String),

    #[error("{0}")]
    HostCommandFailed(String),

    #[error("config error: {0}")]
    ConfigError(String),

    #[error("cannot determine state directory: neither XDG_STATE_HOME nor HOME is set")]
    GlobalStateDirNotFound,

    #[error(
        "`am run` has been removed — for a containerized session it typed into the running \
         container's stdin instead of launching anything. Set `defaults.agent` in \
         .am/config.toml (or run `am setup --agent <name>`), then run `am attach {0}`"
    )]
    RunRemoved(String),

    #[error(transparent)]
    IoError(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_in_repo_formats_correctly() {
        let e = AmError::NotInRepo;
        assert!(e.to_string().contains("git or jj repository"));
    }

    #[test]
    fn not_in_tmux_formats_correctly() {
        let e = AmError::NotInTmux;
        assert!(e.to_string().contains("tmux"));
    }

    #[test]
    fn slug_already_exists_includes_slug() {
        let e = AmError::SlugAlreadyExists("feat".to_string());
        let msg = e.to_string();
        assert!(msg.contains("feat"));
        assert!(msg.contains("am destroy feat"));
    }

    #[test]
    fn slug_not_found_includes_slug() {
        let e = AmError::SlugNotFound("missing".to_string());
        let msg = e.to_string();
        assert!(msg.contains("missing"));
        assert!(msg.contains("am list"));
    }

    #[test]
    fn worktree_error_includes_message() {
        let e = AmError::WorktreeError("branch exists".to_string());
        assert!(e.to_string().contains("branch exists"));
    }

    #[test]
    fn tmux_error_includes_message() {
        let e = AmError::TmuxError("window not found".to_string());
        assert!(e.to_string().contains("window not found"));
    }

    #[test]
    fn container_runtime_not_found_mentions_podman() {
        let e = AmError::ContainerRuntimeNotFound;
        assert!(e.to_string().contains("Podman"));
    }

    #[test]
    fn container_image_not_configured_mentions_agent() {
        let e = AmError::ContainerImageNotConfigured;
        let msg = e.to_string();
        assert!(msg.contains("--agent"));
        assert!(msg.contains("defaults.agent"));
    }

    #[test]
    fn auto_requires_container_mentions_no_container() {
        let e = AmError::AutoRequiresContainer;
        assert!(e.to_string().contains("--no-container"));
    }

    #[test]
    fn auto_requires_agent_mentions_agent() {
        let e = AmError::AutoRequiresAgent;
        assert!(e.to_string().contains("--agent"));
    }

    #[test]
    fn container_error_includes_message() {
        let e = AmError::ContainerError("exit code 1".to_string());
        assert!(e.to_string().contains("exit code 1"));
    }

    #[test]
    fn config_error_includes_message() {
        let e = AmError::ConfigError("invalid toml".to_string());
        assert!(e.to_string().contains("invalid toml"));
    }

    #[test]
    fn run_removed_names_the_replacement_path() {
        let e = AmError::RunRemoved("my-feature".to_string());
        let msg = e.to_string();
        assert!(msg.contains("am run"));
        assert!(msg.contains("defaults.agent"));
        assert!(msg.contains("am setup --agent"));
        assert!(msg.contains("am attach my-feature"));
    }

    #[test]
    fn io_error_converts_from_std() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let e: AmError = io_err.into();
        assert!(e.to_string().contains("file missing"));
    }
}
