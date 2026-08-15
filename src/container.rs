use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::color;
use crate::command::shell_quote;
use crate::config::{NetworkMode, RuntimePreference, Vcs};
use crate::error::AmError;

// Path handling strategy (preserve type safety as long as possible):
// - Keep as Path/PathBuf in internal code
// - Use &Path in function parameters (not &str)
// - Convert to String only at boundaries (Command args, logging, display)
// - Prefer .display() for format strings (never panics, handles UTF-8)
// - Use .to_string_lossy() only when String ownership is needed
// - Use .to_str()? only for critical UTF-8 requirements with error handling

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeKind {
    Podman,
    Docker,
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeKind::Podman => write!(f, "podman"),
            RuntimeKind::Docker => write!(f, "docker"),
        }
    }
}

/// A known agent preset. Adding a new variant here causes exhaustive-match
/// errors in all agent-specific functions below, enforcing that every site
/// is kept in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownAgent {
    Claude,
    Copilot,
    Gemini,
    Codex,
}

impl KnownAgent {
    /// Parse a string into a `KnownAgent`, returning a descriptive error for
    /// unknown names. This replaces the old `validate_agent_name` function.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "claude" => Ok(KnownAgent::Claude),
            "copilot" => Ok(KnownAgent::Copilot),
            "gemini" => Ok(KnownAgent::Gemini),
            "codex" => Ok(KnownAgent::Codex),
            unknown => Err(AmError::ConfigError(format!(
                "unknown agent '{unknown}' — valid agents are: claude, copilot, gemini, codex",
            ))
            .into()),
        }
    }
}

impl std::fmt::Display for KnownAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KnownAgent::Claude => write!(f, "claude"),
            KnownAgent::Copilot => write!(f, "copilot"),
            KnownAgent::Gemini => write!(f, "gemini"),
            KnownAgent::Codex => write!(f, "codex"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContainerRuntime {
    pub kind: RuntimeKind,
    pub bin: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentAuthMount {
    pub host_path: PathBuf,
    pub container_path: PathBuf,
    pub mode: MountMode,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentAuth {
    pub mounts: Vec<AgentAuthMount>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct ContainerMounts {
    pub worktree_host: PathBuf,
    pub vcs_host: PathBuf,                   // .git dir (git) or .jj dir (jj)
    pub colocated_git_host: Option<PathBuf>, // .git for colocated jj+git repos
    pub gitconfig_host: PathBuf,             // $XDG_STATE_HOME/am/gitconfig (or override)
    pub ssh_host: PathBuf,                   // ~/.ssh
    /// The host's `SSH_AUTH_SOCK`, when `container.ssh_agent` is on and the variable is
    /// set. `None` disables forwarding entirely.
    pub ssh_agent_sock: Option<PathBuf>,
    pub agent_auth: Vec<AgentAuthMount>,
    /// Home directory inside the container. Derived from the configured container user
    /// unless a devcontainer's `remoteUser` or an explicit override says otherwise.
    /// Everything user-dependent resolves through this, so the username itself is not
    /// carried any further.
    pub container_home: String,
}

/// Derive the home directory for a user inside a container.
///
/// `root` is the case worth special-casing: devcontainer images frequently run as root,
/// and `/home/root` does not exist, so credential mounts would land somewhere the agent
/// never looks.
pub fn container_home(user: &str, override_home: Option<&Path>) -> String {
    if let Some(path) = override_home {
        return path.to_string_lossy().into_owned();
    }
    if user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{user}")
    }
}

/// Runtime settings contributed by a devcontainer config.
///
/// Everything here has already passed the trust gate — `build_run_command` applies what it
/// is given and does not second-guess it, so the decision about escalating options lives in
/// exactly one place rather than being spread across the command builder.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevcontainerRuntime {
    /// `containerEnv` and `remoteEnv`, already merged and substituted.
    pub env: Vec<(String, String)>,
    pub mounts: Vec<crate::devcontainer::NormalizedMount>,
    pub init: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    /// Raw runtime flags from `runArgs`.
    pub run_args: Vec<String>,
    /// `workspaceFolder`, overriding the mirrored host path as the working directory.
    pub workdir: Option<String>,
    /// Feature entrypoint scripts, composed ahead of the agent command.
    pub entrypoints: Vec<String>,
    /// How to derive the agent's environment, from `userEnvProbe`.
    pub user_env_probe: crate::devcontainer::UserEnvProbe,
    /// Ports to publish on the host, from `forwardPorts`.
    pub ports: Vec<crate::devcontainer::ForwardedPort>,
    /// The user to run as, from `remoteUser`/`containerUser`.
    ///
    /// Without this the container runs as the image's default user — root for most
    /// devcontainer images — and `$HOME` is `/root`, so the credentials `am` mounts at
    /// the `remoteUser`'s home are invisible to the agent.
    pub user: Option<String>,
}

impl DevcontainerRuntime {
    /// Runtime flags for image mode, where there is no devcontainer config to read them
    /// from.
    ///
    /// `init` is on: without an init process, PID 1 is the agent, which only waits on
    /// children it spawned itself. Anything the kernel re-parents to it — git's auto-gc
    /// detaches on purpose, and long agent sessions run a lot of git — stays a zombie
    /// holding a PID slot until the container exits. In devcontainer mode this is the
    /// config's call, and `am` leaves it alone.
    pub fn image_mode() -> Self {
        Self {
            init: true,
            ..Self::default()
        }
    }
}

// ── Runtime detection ─────────────────────────────────────────────────────────

fn find_bin(name: &str, env_override: &str) -> Option<PathBuf> {
    // If the env var is set, use it exclusively — don't fall back to which.
    // This lets tests inject a nonexistent path to simulate "not found".
    if let Ok(path) = std::env::var(env_override) {
        let p = PathBuf::from(path);
        return if p.exists() { Some(p) } else { None };
    }
    which::which(name).ok()
}

pub fn detect_runtime(preference: RuntimePreference) -> Result<ContainerRuntime> {
    match preference {
        RuntimePreference::Auto => {
            if let Some(bin) = find_bin("podman", "AM_PODMAN_BIN") {
                return Ok(ContainerRuntime {
                    kind: RuntimeKind::Podman,
                    bin,
                });
            }
            if let Some(bin) = find_bin("docker", "AM_DOCKER_BIN") {
                return Ok(ContainerRuntime {
                    kind: RuntimeKind::Docker,
                    bin,
                });
            }
            Err(AmError::ContainerRuntimeNotFound.into())
        }
        RuntimePreference::Podman => find_bin("podman", "AM_PODMAN_BIN")
            .map(|bin| ContainerRuntime {
                kind: RuntimeKind::Podman,
                bin,
            })
            .ok_or_else(|| AmError::RequestedContainerRuntimeNotFound("podman".to_string()).into()),
        RuntimePreference::Docker => find_bin("docker", "AM_DOCKER_BIN")
            .map(|bin| ContainerRuntime {
                kind: RuntimeKind::Docker,
                bin,
            })
            .ok_or_else(|| AmError::RequestedContainerRuntimeNotFound("docker".to_string()).into()),
    }
}

// ── SELinux label ─────────────────────────────────────────────────────────────

pub(crate) fn use_selinux_labels(runtime: &ContainerRuntime) -> bool {
    cfg!(target_os = "linux") && runtime.kind == RuntimeKind::Podman
}

pub(crate) fn mount_str(host: &Path, container: &str, mode: MountMode, selinux: bool) -> String {
    let mode_str = match mode {
        MountMode::ReadOnly => "ro",
        MountMode::ReadWrite => "rw",
    };
    if selinux {
        format!("{}:{}:{},z", host.display(), container, mode_str)
    } else {
        format!("{}:{}:{}", host.display(), container, mode_str)
    }
}

// ── Mount resolution ──────────────────────────────────────────────────────────

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME").map(PathBuf::from).with_context(|| {
        "HOME environment variable not set — cannot resolve user home directory for mounts"
    })
}

/// The host's SSH agent socket, if there is one.
///
/// Not an error when unset: plenty of hosts have no agent running, and a session that
/// cannot reach one is only degraded, not broken.
fn ssh_auth_sock() -> Option<PathBuf> {
    std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_mounts(
    slug: &str,
    repo_root: &Path,
    vcs: &Vcs,
    agent_auth: Vec<AgentAuthMount>,
    gitconfig: Option<&Path>,
    ssh: Option<&Path>,
    ssh_agent: bool,
    container_user: &str,
    home_override: Option<&Path>,
) -> Result<ContainerMounts> {
    let home = home_dir()?;
    let worktree_host = repo_root.join(".am").join("worktrees").join(slug);
    let vcs_host = match vcs {
        Vcs::Git => repo_root.join(".git"),
        Vcs::Jj => repo_root.join(".jj"),
    };
    // For colocated jj+git repos, .git holds the git object store used as the
    // jj backend and must be mounted alongside .jj.
    let colocated_git_host = if matches!(vcs, Vcs::Jj) {
        let git = repo_root.join(".git");
        if git.is_dir() {
            Some(git)
        } else {
            None
        }
    } else {
        None
    };
    let gitconfig_host = gitconfig
        .map(|p| p.to_path_buf())
        .or_else(|| crate::config::global_state_dir().map(|d| d.join("gitconfig")))
        .unwrap_or_else(|| repo_root.join(".am").join("gitconfig"));
    let ssh_host = ssh
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| home.join(".ssh"));

    Ok(ContainerMounts {
        worktree_host,
        vcs_host,
        colocated_git_host,
        gitconfig_host,
        ssh_host,
        ssh_agent_sock: ssh_agent.then(ssh_auth_sock).flatten(),
        agent_auth,
        container_home: container_home(container_user, home_override),
    })
}

/// Where each agent keeps its credentials inside the container.
///
/// `home_in_container` is passed in rather than derived from the username: a devcontainer's
/// `remoteUser` may be `root` (home `/root`, not `/home/root`), and mounting credentials at
/// a path the agent never reads fails silently at the worst moment.
fn resolve_agent_auth_mounts(
    agent: KnownAgent,
    home_in_container: &str,
) -> Result<Vec<AgentAuthMount>> {
    Ok(match agent {
        KnownAgent::Claude => {
            let home = home_dir()?;
            // Config dir: use CLAUDE_CONFIG_DIR if set, otherwise ~/.claude
            let config_host = std::env::var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join(".claude"));
            vec![
                AgentAuthMount {
                    host_path: config_host,
                    container_path: PathBuf::from(format!("{home_in_container}/.claude")),
                    mode: MountMode::ReadWrite,
                },
                AgentAuthMount {
                    host_path: home.join(".claude.json"),
                    container_path: PathBuf::from(format!("{home_in_container}/.claude.json")),
                    mode: MountMode::ReadWrite,
                },
            ]
        }
        KnownAgent::Copilot => {
            let home = home_dir()?;
            vec![
                AgentAuthMount {
                    // GitHub CLI auth token (required for Copilot authentication)
                    host_path: home.join(".config").join("gh"),
                    container_path: PathBuf::from(format!("{home_in_container}/.config/gh")),
                    mode: MountMode::ReadOnly,
                },
                AgentAuthMount {
                    host_path: home.join(".config").join("github-copilot"),
                    container_path: PathBuf::from(format!(
                        "{home_in_container}/.config/github-copilot"
                    )),
                    mode: MountMode::ReadOnly,
                },
            ]
        }
        KnownAgent::Gemini => {
            let home = home_dir()?;
            vec![AgentAuthMount {
                host_path: home.join(".gemini"),
                container_path: PathBuf::from(format!("{home_in_container}/.gemini")),
                mode: MountMode::ReadOnly,
            }]
        }
        KnownAgent::Codex => {
            // Unresolvable HOME is not fatal here, unlike the other agents: codex may be
            // authenticated by an API key alone, and failing the whole preflight because
            // there is nowhere to look for a sign-in would break that case.
            let Ok(home) = home_dir() else {
                return Ok(vec![]);
            };
            let config_host = home.join(".codex");
            // Only mount what exists: an API-key user may never have run codex on this
            // host, and mounting a missing directory would have the runtime create it
            // as root-owned.
            if config_host.exists() {
                vec![AgentAuthMount {
                    // The whole directory, read-write. Codex signs in interactively and
                    // rotates the token in auth.json, which it replaces rather than
                    // rewrites — a single-file mount would leave the container writing
                    // to a detached inode the host never sees. Read-only would work
                    // until the first token refresh and then fail.
                    host_path: config_host,
                    container_path: PathBuf::from(format!("{home_in_container}/.codex")),
                    mode: MountMode::ReadWrite,
                }]
            } else {
                vec![]
            }
        }
    })
}

/// Returns the extra CLI flags needed to run an agent in autonomous mode.
pub fn agent_auto_flags(agent: KnownAgent) -> Vec<String> {
    match agent {
        KnownAgent::Claude => vec!["--dangerously-skip-permissions".to_string()],
        KnownAgent::Copilot | KnownAgent::Gemini | KnownAgent::Codex => vec![],
    }
}

/// The flags needed to ask an agent CLI to resume its previous conversation, or `None` for
/// an agent confirmed not to support that. `am attach` appends these (OQ-3/OQ-4) instead of
/// launching a fresh conversation, unless `--fresh` or `[attach].resume = false` says
/// otherwise. `None` must never be treated as an error — it just means a fresh launch, same
/// as today's behavior.
///
/// Every entry here was checked against the CLI's own `--help` output (not assumed) as of
/// this writing:
/// - `claude --help`: `-c, --continue` — "Continue the most recent conversation in the
///   current directory" (run locally; the Claude Code CLI is present in this environment).
/// - `npx @github/copilot --help`: `--continue` — "Resume the most recent session".
/// - `npx @google/gemini-cli --help`: `-r, --resume <value>` — "Resume a previous session.
///   Use \"latest\" for most recent or index number".
/// - `npx @openai/codex resume --help`: `resume` is a subcommand, not a flag on the base
///   invocation; `--last` — "Continue the most recent session without showing the picker" —
///   and that selection is itself scoped to the current working directory by default (its
///   sibling `--all` flag is documented as "disables cwd filtering").
pub fn agent_resume_flags(agent: KnownAgent) -> Option<Vec<String>> {
    match agent {
        KnownAgent::Claude => Some(vec!["--continue".to_string()]),
        KnownAgent::Copilot => Some(vec!["--continue".to_string()]),
        KnownAgent::Gemini => Some(vec!["--resume".to_string(), "latest".to_string()]),
        // A subcommand, not a flag — `agent_command` puts this right after the binary
        // name, giving `codex resume --last`.
        KnownAgent::Codex => Some(vec!["resume".to_string(), "--last".to_string()]),
    }
}

/// Runs `gh auth token` and returns the token string.
fn get_gh_token() -> Result<String> {
    let gh = find_bin("gh", "AM_GH_BIN").ok_or_else(|| {
        anyhow::anyhow!("failed to execute 'gh auth token' — is GitHub CLI installed?")
    })?;
    let output = std::process::Command::new(gh)
        .args(["auth", "token"])
        .output()
        .with_context(|| "failed to execute 'gh auth token' — is GitHub CLI installed?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow::anyhow!(
            "GitHub CLI authentication failed — run 'gh auth login' to authenticate\n\
             Error: {stderr}"
        ))
        .with_context(|| "retrieving GitHub authentication token for Copilot");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Codex is authenticated by an API key *or* an interactive sign-in, so neither missing
/// one is an error on its own — only both together. Naming both in one message keeps the
/// user from fixing the half they were not using.
fn codex_credentials_error(agent: KnownAgent) -> anyhow::Error {
    anyhow::anyhow!(
        "agent '{agent}' has no credentials: OPENAI_API_KEY is not set and ~/.codex does not exist\n\
         Run 'codex' once to sign in, or export OPENAI_API_KEY=sk-..."
    )
}

fn ensure_required_paths(agent: KnownAgent, required: &[PathBuf]) -> Result<()> {
    for path in required {
        if !path.exists() {
            return Err(anyhow::anyhow!(
                "agent '{agent}' requires path to exist: {path}\n\
                 Make sure {agent} is installed and authenticated on this system",
                path = path.display()
            ))
            .with_context(|| {
                format!(
                    "checking agent credentials for '{agent}' at {}",
                    path.display()
                )
            });
        }
    }
    Ok(())
}

fn resolve_agent_auth(agent: KnownAgent, home_in_container: &str) -> Result<AgentAuth> {
    match agent {
        KnownAgent::Claude => {
            let mounts = resolve_agent_auth_mounts(agent, home_in_container)?;
            let required = mounts
                .first()
                .map(|mount| vec![mount.host_path.clone()])
                .unwrap_or_default();
            ensure_required_paths(agent, &required)?;
            Ok(AgentAuth {
                mounts,
                env: vec![],
            })
        }
        KnownAgent::Copilot => {
            let mounts = resolve_agent_auth_mounts(agent, home_in_container)?;
            let required = mounts
                .iter()
                .find(|mount| mount.host_path.ends_with(Path::new(".config/gh")))
                .map(|mount| vec![mount.host_path.clone()])
                .unwrap_or_default();
            ensure_required_paths(agent, &required)?;
            let token = get_gh_token()?;
            Ok(AgentAuth {
                mounts,
                env: vec![("GH_TOKEN".to_string(), token)],
            })
        }
        KnownAgent::Gemini => {
            let mounts = resolve_agent_auth_mounts(agent, home_in_container)?;
            let required = mounts
                .first()
                .map(|mount| vec![mount.host_path.clone()])
                .unwrap_or_default();
            ensure_required_paths(agent, &required)?;
            Ok(AgentAuth {
                mounts,
                env: vec![],
            })
        }
        KnownAgent::Codex => {
            // Two independent ways to be authenticated, and either is sufficient: an
            // API key in the environment, or an interactive sign-in codex persisted to
            // ~/.codex. Requiring the key locked out every signed-in user.
            let mounts = resolve_agent_auth_mounts(agent, home_in_container)?;
            let key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty());
            if mounts.is_empty() && key.is_none() {
                return Err(codex_credentials_error(agent));
            }
            Ok(AgentAuth {
                mounts,
                env: key
                    .map(|value| vec![("OPENAI_API_KEY".to_string(), value)])
                    .unwrap_or_default(),
            })
        }
    }
}

/// Resolve and validate a known agent's authentication requirements before the
/// container is launched. This performs all preflight checks and returns the
/// mounts and environment variables needed for the actual runtime command.
pub fn preflight_agent_auth(agent: KnownAgent, home_in_container: &str) -> Result<AgentAuth> {
    resolve_agent_auth(agent, home_in_container)
}

/// Check that an agent's credentials exist on the host, without deciding where they will
/// be mounted.
///
/// This exists so credential problems surface *before* `am start` creates a worktree. The
/// mount targets cannot be known that early in devcontainer mode — they depend on the
/// `remoteUser` recorded in an image that has not been built yet — but whether the user is
/// logged in does not depend on any of that.
pub fn validate_agent_credentials(agent: KnownAgent) -> Result<()> {
    match agent {
        KnownAgent::Claude => {
            let config_host = std::env::var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home_dir().unwrap_or_default().join(".claude"));
            ensure_required_paths(agent, &[config_host])
        }
        KnownAgent::Copilot => {
            ensure_required_paths(agent, &[home_dir()?.join(".config").join("gh")])
        }
        KnownAgent::Gemini => ensure_required_paths(agent, &[home_dir()?.join(".gemini")]),
        KnownAgent::Codex => {
            // Either form of credential is enough. Codex accepts an API key from the
            // environment *or* an interactive sign-in it persists to ~/.codex/auth.json,
            // and requiring the env var locked out everyone who uses the second — the
            // agent worked on the host and failed the moment am wrapped it.
            let has_signin = home_dir()
                .map(|home| home.join(".codex").join("auth.json").exists())
                .unwrap_or(false);
            let has_key = std::env::var("OPENAI_API_KEY")
                .ok()
                .is_some_and(|value| !value.trim().is_empty());
            if has_signin || has_key {
                Ok(())
            } else {
                Err(codex_credentials_error(agent))
            }
        }
    }
}

/// A concrete, agent-specific instruction for a credentials failure — presence-only, the
/// same guarantee `validate_agent_credentials` itself makes; never prints or implies
/// anything about whether the credentials found are still *valid*, only how to obtain some.
/// Used exclusively as `doctor::check_agent`'s `Status::Fail` hint.
///
/// Points at the published docs site rather than a repo-relative path — most users run an
/// installed binary and never cloned the repo, so `docs/guides/codex.md#prerequisites` is
/// meaningless to them. `mkdocs.yml` sets `site_url` and leaves `use_directory_urls` at its
/// default of `true`, so `docs/guides/<name>.md#<anchor>` publishes as
/// `<site_url>/guides/<name>/#<anchor>` — verified against a local `mkdocs build`.
pub fn credentials_hint(agent: KnownAgent) -> &'static str {
    match agent {
        KnownAgent::Claude => {
            "run 'claude auth login' (or set ANTHROPIC_API_KEY) — see \
             https://dstanek.github.io/agent-manager/guides/claude-code/#prerequisites"
        }
        KnownAgent::Copilot => {
            "run 'gh auth login' — see \
             https://dstanek.github.io/agent-manager/guides/github-copilot/#prerequisites"
        }
        KnownAgent::Gemini => {
            "authenticate with the Gemini CLI on this host — see \
             https://dstanek.github.io/agent-manager/guides/gemini/#prerequisites"
        }
        KnownAgent::Codex => {
            "run 'codex' once to sign in (or set OPENAI_API_KEY) — see \
             https://dstanek.github.io/agent-manager/guides/codex/#prerequisites"
        }
    }
}

// ── Command building ──────────────────────────────────────────────────────────

#[cfg(unix)]
pub(crate) fn get_host_uid_gid() -> Option<(u32, u32)> {
    extern "C" {
        fn getuid() -> u32;
        fn getgid() -> u32;
    }
    // SAFETY: getuid/getgid have no preconditions and are always safe to call.
    Some(unsafe { (getuid(), getgid()) })
}

#[cfg(not(unix))]
fn get_host_uid_gid() -> Option<(u32, u32)> {
    None
}

/// Read a single value out of a specific gitconfig file. Delegates the parsing to
/// `git` itself so includes and conditional includes resolve the way they would for
/// the user, rather than being re-implemented here.
fn read_gitconfig_value(path: &Path, key: &str) -> Option<String> {
    std::process::Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(path)
        .arg(key)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Derive `JJ_USER`/`JJ_EMAIL` from the gitconfig that gets mounted into the container.
///
/// jj does not read git's identity, so without this a jj commit made inside a session
/// lands with an empty committer and jj itself refuses to push it. Deriving both from
/// the same file keeps git and jj agreeing on who you are, including when the user has
/// pointed `container.gitconfig` at a gitconfig of their own.
///
/// Returns nothing unless *both* values are present — a half-populated identity would
/// produce the same unpushable commit while looking like it had been configured.
pub(crate) fn jj_identity_env(gitconfig: &Path) -> Vec<(String, String)> {
    if !gitconfig.exists() {
        return Vec::new();
    }
    match (
        read_gitconfig_value(gitconfig, "user.name"),
        read_gitconfig_value(gitconfig, "user.email"),
    ) {
        (Some(name), Some(email)) => vec![
            ("JJ_USER".to_string(), name),
            ("JJ_EMAIL".to_string(), email),
        ],
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_run_command(
    runtime: &ContainerRuntime,
    image: &str,
    mounts: &ContainerMounts,
    env_passthrough: &[String],
    extra_env: &[(String, String)],
    network: &NetworkMode,
    container_name: &str,
    dc: &DevcontainerRuntime,
) -> Vec<String> {
    let home = &mounts.container_home;
    let selinux = use_selinux_labels(runtime);
    let mut cmd = vec![
        runtime.bin.to_string_lossy().into_owned(),
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        container_name.to_string(),
    ];

    // Run as the host user so bind-mounted files are readable/writable
    if let Some((uid, gid)) = get_host_uid_gid() {
        match runtime.kind {
            RuntimeKind::Podman => {
                cmd.push(format!("--userns=keep-id:uid={uid},gid={gid}"));
            }
            RuntimeKind::Docker => {
                // A named devcontainer user takes precedence over the numeric mapping;
                // devcontainer images give that user uid 1000, which is the same mapping
                // by another name.
                if dc.user.is_none() {
                    cmd.push("--user".to_string());
                    cmd.push(format!("{uid}:{gid}"));
                }
            }
        }
    }

    // Run as the devcontainer's remoteUser rather than the image's default (usually root),
    // so $HOME matches where the credential mounts land.
    if let Some(ref user) = dc.user {
        cmd.push("--user".to_string());
        cmd.push(user.clone());
    }

    // Worktree mount — same path inside the container as on the host
    cmd.push("-v".to_string());
    cmd.push(mount_str(
        &mounts.worktree_host,
        &mounts.worktree_host.to_string_lossy(),
        MountMode::ReadWrite,
        selinux,
    ));

    // VCS dir mount — same path inside the container as on the host
    cmd.push("-v".to_string());
    cmd.push(mount_str(
        &mounts.vcs_host,
        &mounts.vcs_host.to_string_lossy(),
        MountMode::ReadWrite,
        selinux,
    ));

    // Colocated jj+git: mount the git object store alongside .jj
    if let Some(ref git) = mounts.colocated_git_host {
        cmd.push("-v".to_string());
        cmd.push(mount_str(
            git,
            &git.to_string_lossy(),
            MountMode::ReadWrite,
            selinux,
        ));
    }

    // ~/.gitconfig — only mount if the file exists
    if mounts.gitconfig_host.exists() {
        cmd.push("-v".to_string());
        cmd.push(mount_str(
            &mounts.gitconfig_host,
            &format!("{home}/.gitconfig"),
            MountMode::ReadOnly,
            selinux,
        ));
    }

    // ~/.ssh — only mount if the directory exists
    if mounts.ssh_host.exists() {
        cmd.push("-v".to_string());
        cmd.push(mount_str(
            &mounts.ssh_host,
            &format!("{home}/.ssh"),
            MountMode::ReadOnly,
            selinux,
        ));
    }

    // SSH agent socket — same path inside the container as on the host, so SSH_AUTH_SOCK
    // carries over unchanged. Read-write, because connecting to a unix socket needs it.
    //
    // Deliberately not relabelled even under SELinux: the socket belongs to the host's
    // agent process, and `:z` would rewrite the label on a file the host still needs.
    if let Some(sock) = &mounts.ssh_agent_sock {
        if sock.exists() {
            cmd.push("-v".to_string());
            cmd.push(mount_str(
                sock,
                &sock.to_string_lossy(),
                MountMode::ReadWrite,
                false,
            ));
            cmd.push("-e".to_string());
            cmd.push(format!("SSH_AUTH_SOCK={}", sock.to_string_lossy()));
        }
    }

    // Agent auth mounts — only mount if the path exists
    for auth in &mounts.agent_auth {
        if auth.host_path.exists() {
            cmd.push("-v".to_string());
            cmd.push(mount_str(
                &auth.host_path,
                &auth.container_path.to_string_lossy(),
                auth.mode.clone(),
                selinux,
            ));
        }
    }

    // Devcontainer mounts. These come from the repo's config and its Features, so they
    // are labelled for SELinux like am's own mounts rather than passed through raw.
    for mount in &dc.mounts {
        let Some(ref source) = mount.source else {
            // A bind with no source is meaningless and a volume with no name is
            // anonymous — neither is worth guessing at.
            continue;
        };
        let mode = if mount.read_only {
            MountMode::ReadOnly
        } else {
            MountMode::ReadWrite
        };
        cmd.push("-v".to_string());
        // Only bind mounts name a host path; volumes name a runtime-managed volume and
        // must not be relabelled.
        if mount.kind == "bind" {
            cmd.push(mount_str(Path::new(source), &mount.target, mode, selinux));
        } else {
            let mode_str = if mount.read_only { "ro" } else { "rw" };
            cmd.push(format!("{source}:{}:{mode_str}", mount.target));
        }
    }

    // jj identity, derived from the gitconfig mounted above. Emitted before the
    // other env sources so an explicit JJ_USER/JJ_EMAIL from config, a devcontainer,
    // or host pass-through still wins.
    for (key, val) in jj_identity_env(&mounts.gitconfig_host) {
        cmd.push("-e".to_string());
        cmd.push(format!("{key}={val}"));
    }

    // Extra env vars (e.g. agent-specific tokens)
    for (key, val) in extra_env {
        cmd.push("-e".to_string());
        cmd.push(format!("{key}={val}"));
    }

    // Devcontainer containerEnv/remoteEnv
    for (key, val) in &dc.env {
        cmd.push("-e".to_string());
        cmd.push(format!("{key}={val}"));
    }

    // Host env pass-through
    for var in env_passthrough {
        cmd.push("-e".to_string());
        cmd.push(var.clone());
    }

    // Escalating options from the devcontainer config. These have already passed the
    // trust gate; anything the user declined was dropped before we got here.
    if dc.init {
        cmd.push("--init".to_string());
    }
    if dc.privileged {
        cmd.push("--privileged".to_string());
    }
    for cap in &dc.cap_add {
        cmd.push("--cap-add".to_string());
        cmd.push(cap.clone());
    }
    for opt in &dc.security_opt {
        cmd.push("--security-opt".to_string());
        cmd.push(opt.clone());
    }

    // forwardPorts. The reference CLI leaves these to an editor and publishes nothing; am has
    // no editor, so publishing is the only way the key means anything here. Loopback-bound,
    // which is the conservative reading of "forward this to me" and matches what the CLI does
    // for a bare `appPort`.
    //
    // A `"<service>:<port>"` entry names another compose service, so it has no meaning for a
    // single container and is skipped rather than guessed at.
    for port in &dc.ports {
        if let crate::devcontainer::ForwardedPort::Own(p) = port {
            cmd.push("-p".to_string());
            cmd.push(crate::devcontainer::ForwardedPort::publish_spec(*p));
        }
    }

    // Network. Applied after runArgs would be, so am's own setting stays authoritative
    // over a config that tries to widen it.
    cmd.extend(dc.run_args.iter().cloned());
    if matches!(network, NetworkMode::None) {
        cmd.push("--network".to_string());
        cmd.push("none".to_string());
    }

    // Working directory — the worktree's host path, unless the config names another.
    cmd.push("--workdir".to_string());
    cmd.push(match dc.workdir {
        Some(ref dir) => dir.clone(),
        None => mounts.worktree_host.to_string_lossy().into_owned(),
    });

    cmd.push(image.to_string());
    cmd
}

/// Compose feature entrypoints and the agent into a single container command.
///
/// Features contribute entrypoint scripts that must run before the agent — starting a
/// docker daemon, an SSH server. `am` overrides the image's `ENTRYPOINT` to launch the
/// agent, so those scripts have to be chained explicitly or they never run at all.
///
/// `exec` on the last element matters: it makes the agent PID 1's direct child, so signals
/// and exit codes propagate instead of being swallowed by an intermediate shell.
///
/// `probe` runs `userEnvProbe` ahead of the agent; `protected` names the variables `am` set
/// deliberately, which the probe must not overwrite.
pub fn compose_entrypoint_command(
    entrypoints: &[String],
    agent_cmd: &[String],
    probe: crate::devcontainer::UserEnvProbe,
    protected: &[String],
) -> Vec<String> {
    let probe_script = user_env_probe_script(probe, protected);
    if entrypoints.is_empty() && probe_script.is_none() {
        return agent_cmd.to_vec();
    }

    // Entrypoints stay `&&`-chained through to the agent: a Feature's init script failing must
    // stop the session rather than launch an agent into a half-built container. The probe is
    // joined with a newline instead, because finding no variables is not a failure.
    let mut tail = entrypoints.join(" && ");
    if !agent_cmd.is_empty() {
        let quoted = agent_cmd
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        if tail.is_empty() {
            tail = format!("exec {quoted}");
        } else {
            tail.push_str(&format!(" && exec {quoted}"));
        }
    }

    let script = match probe_script {
        Some(probe) if tail.is_empty() => probe,
        Some(probe) => format!("{probe}\n{tail}"),
        None => tail,
    };
    vec!["sh".to_string(), "-c".to_string(), script]
}

/// The environment variables `am` sets deliberately, which `userEnvProbe` must not overwrite.
///
/// Derived from the same inputs [`build_run_command`] emits `-e` flags for, so the two cannot
/// drift: a variable added there without being added here would become one a dotfile can
/// silently override.
pub fn protected_env_names(
    mounts: &ContainerMounts,
    env_passthrough: &[String],
    extra_env: &[(String, String)],
    dc: &DevcontainerRuntime,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        let name = name.to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    };
    if mounts.ssh_agent_sock.as_ref().is_some_and(|s| s.exists()) {
        push("SSH_AUTH_SOCK");
    }
    for (key, _) in jj_identity_env(&mounts.gitconfig_host) {
        push(&key);
    }
    for (key, _) in extra_env {
        push(key);
    }
    for (key, _) in &dc.env {
        push(key);
    }
    for var in env_passthrough {
        push(var.split_once('=').map_or(var.as_str(), |(k, _)| k));
    }
    names
}

/// The shell snippet that runs `userEnvProbe` and applies what it finds.
///
/// Mirrors the reference CLI: resolve the user's login shell, run it with the mode's flags, and
/// read `/proc/self/environ` — NUL-separated, so a value containing a newline survives the trip
/// out of the shell even though the loop below is line-based.
///
/// Two deliberate differences from a naive "run the agent under a login shell". The probe is a
/// throwaway process, so a `.bashrc` that prints a banner or starts a job-control message does
/// not end up in the agent's own process tree. And variables `am` set on purpose —
/// `containerEnv`, `remoteEnv`, agent credentials, the jj identity — are skipped, so a dotfile
/// cannot quietly undo the session's configuration.
fn user_env_probe_script(
    probe: crate::devcontainer::UserEnvProbe,
    protected: &[String],
) -> Option<String> {
    let flags = probe.shell_flags()?;
    // A `case` pattern rather than a loop: it is one comparison per variable in the generated
    // script instead of one per protected name.
    let guard = if protected.is_empty() {
        String::new()
    } else {
        let names = protected
            .iter()
            .map(|n| format!("{n}=*"))
            .collect::<Vec<_>>()
            .join("|");
        format!("    case \"$_am_line\" in {names}) continue ;; esac\n")
    };
    Some(format!(
        "_am_shell=$(getent passwd \"$(id -u)\" 2>/dev/null | cut -d: -f7)\n\
         [ -x \"$_am_shell\" ] || _am_shell=/bin/sh\n\
         _am_env=$(\"$_am_shell\" {flags} 'cat /proc/self/environ' 2>/dev/null | tr '\\0' '\\n')\n\
         _am_ifs=$IFS; IFS='\n'\n\
         for _am_line in $_am_env; do\n\
         {guard}\
         \x20   case \"$_am_line\" in *=*) export \"$_am_line\" ;; esac\n\
         done\n\
         IFS=$_am_ifs; unset _am_shell _am_env _am_ifs _am_line"
    ))
}


// ── Container lifecycle ───────────────────────────────────────────────────────

fn run_container_cmd(runtime: &ContainerRuntime, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new(&runtime.bin)
        .args(args)
        .output()
        .map_err(|e| AmError::ContainerError(format!("failed to run container command: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AmError::ContainerError(if stderr.is_empty() {
            format!("container command exited with status {}", output.status)
        } else {
            stderr
        })
        .into());
    }
    Ok(())
}

pub fn stop_container(runtime: &ContainerRuntime, container_name: &str) -> Result<()> {
    // Ignore error — container may already be stopped
    let _ = run_container_cmd(runtime, &["stop", container_name]);
    Ok(())
}

pub fn remove_container(runtime: &ContainerRuntime, container_name: &str) -> Result<()> {
    run_container_cmd(runtime, &["rm", "-f", container_name])
}

/// Pre-emptively remove a container with this name (e.g. from a crashed
/// previous session), logging a warning if one existed.
/// NOTE: `podman/docker rm --force` exits 0 even when the container doesn't
/// exist, so we check existence first to avoid false-positive warnings.
pub fn remove_if_exists(runtime: &ContainerRuntime, container_name: &str) {
    let exists = std::process::Command::new(&runtime.bin)
        .args(["container", "inspect", container_name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if exists {
        let _ = run_container_cmd(runtime, &["rm", "-f", container_name]);
        eprintln!(
            "{} removed existing container '{container_name}' from a previous unclean run",
            color::warning_prefix(color::enabled(color::Stream::Stderr))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn agent_auto_flags_claude_returns_skip_permissions() {
        let flags = agent_auto_flags(KnownAgent::Claude);
        assert_eq!(flags, vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn agent_auto_flags_non_claude_agents_return_empty() {
        assert!(agent_auto_flags(KnownAgent::Codex).is_empty());
        assert!(agent_auto_flags(KnownAgent::Copilot).is_empty());
        assert!(agent_auto_flags(KnownAgent::Gemini).is_empty());
    }

    // ── agent_resume_flags (OQ-3) ─────────────────────────────────────────────

    #[test]
    fn agent_resume_flags_claude_uses_continue() {
        assert_eq!(
            agent_resume_flags(KnownAgent::Claude),
            Some(vec!["--continue".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_copilot_uses_continue() {
        assert_eq!(
            agent_resume_flags(KnownAgent::Copilot),
            Some(vec!["--continue".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_gemini_uses_resume_latest() {
        assert_eq!(
            agent_resume_flags(KnownAgent::Gemini),
            Some(vec!["--resume".to_string(), "latest".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_codex_uses_resume_subcommand() {
        assert_eq!(
            agent_resume_flags(KnownAgent::Codex),
            Some(vec!["resume".to_string(), "--last".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_never_returns_none_by_accident() {
        // Every known agent was verified to support resuming (see agent_resume_flags's
        // doc comment); pin that none of them silently regress to "unsupported".
        for agent in [
            KnownAgent::Claude,
            KnownAgent::Copilot,
            KnownAgent::Gemini,
            KnownAgent::Codex,
        ] {
            assert!(agent_resume_flags(agent).is_some(), "{agent} should support resume");
        }
    }

    fn fake_runtime(kind: RuntimeKind, dir: &Path) -> ContainerRuntime {
        // Create a script that records its args and exits 0
        let bin = dir.join("mock_runtime");
        std::fs::write(&bin, "#!/bin/sh\necho \"$*\" >> \"$MOCK_CONTAINER_LOG\"\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        ContainerRuntime { kind, bin }
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let bin = dir.join(name);
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn fake_gh(dir: &Path, body: &str) -> PathBuf {
        let bin = dir.join("gh");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn make_mounts(tmp: &Path) -> ContainerMounts {
        ContainerMounts {
            worktree_host: tmp.join("worktrees/feat"),
            vcs_host: tmp.join(".git"),
            colocated_git_host: None,
            gitconfig_host: tmp.join(".gitconfig"),
            ssh_host: tmp.join(".ssh"),
            ssh_agent_sock: None,
            agent_auth: vec![],
            container_home: "/home/am".to_string(),
        }
    }

    // ── detect_runtime ────────────────────────────────────────────────────────

    #[test]
    fn detect_runtime_auto_finds_podman() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let podman = fake_bin(tmp.path(), "podman");
        std::env::set_var("AM_PODMAN_BIN", &podman);
        std::env::remove_var("AM_DOCKER_BIN");

        let rt = detect_runtime(RuntimePreference::Auto).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Podman);
        assert_eq!(rt.bin, podman);

        std::env::remove_var("AM_PODMAN_BIN");
    }

    #[test]
    fn detect_runtime_auto_falls_back_to_docker() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let docker = fake_bin(tmp.path(), "docker");
        std::env::set_var("AM_PODMAN_BIN", "/nonexistent/podman");
        std::env::set_var("AM_DOCKER_BIN", &docker);

        let rt = detect_runtime(RuntimePreference::Auto).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Docker);

        std::env::remove_var("AM_PODMAN_BIN");
        std::env::remove_var("AM_DOCKER_BIN");
    }

    #[test]
    fn detect_runtime_auto_errors_when_neither_found() {
        let _g = lock_env();
        std::env::set_var("AM_PODMAN_BIN", "/nonexistent/podman");
        std::env::set_var("AM_DOCKER_BIN", "/nonexistent/docker");

        let err = detect_runtime(RuntimePreference::Auto).unwrap_err();
        assert!(err.to_string().contains("Podman"));

        std::env::remove_var("AM_PODMAN_BIN");
        std::env::remove_var("AM_DOCKER_BIN");
    }

    #[test]
    fn detect_runtime_explicit_podman_errors_when_not_found() {
        let _g = lock_env();
        std::env::set_var("AM_PODMAN_BIN", "/nonexistent/podman");

        let err = detect_runtime(RuntimePreference::Podman).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("podman"));

        std::env::remove_var("AM_PODMAN_BIN");
    }

    #[test]
    fn detect_runtime_explicit_docker_errors_when_not_found() {
        let _g = lock_env();
        std::env::set_var("AM_DOCKER_BIN", "/nonexistent/docker");

        let err = detect_runtime(RuntimePreference::Docker).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("docker"));

        std::env::remove_var("AM_DOCKER_BIN");
    }

    #[test]
    fn detect_runtime_explicit_docker_finds_docker() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let docker = fake_bin(tmp.path(), "docker");
        std::env::set_var("AM_DOCKER_BIN", &docker);

        let rt = detect_runtime(RuntimePreference::Docker).unwrap();
        assert_eq!(rt.kind, RuntimeKind::Docker);
        assert_eq!(rt.bin, docker);

        std::env::remove_var("AM_DOCKER_BIN");
    }

    // ── resolve_mounts ────────────────────────────────────────────────────────

    #[test]
    fn resolve_mounts_git_paths() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let repo_root = tmp.path().join("repo");
        let mounts =
            resolve_mounts(
            "feat",
            &repo_root,
            &Vcs::Git,
            vec![],
            None,
            None,
            false,
            "am",
            None,
        ).unwrap();

        assert_eq!(mounts.worktree_host, repo_root.join(".am/worktrees/feat"));
        assert_eq!(mounts.vcs_host, repo_root.join(".git"));
        // When no explicit gitconfig is given, falls back to global state dir.
        assert_eq!(
            mounts.gitconfig_host,
            tmp.path().join(".local/state/am/gitconfig")
        );
        assert_eq!(mounts.ssh_host, tmp.path().join(".ssh"));
        assert!(mounts.agent_auth.is_empty());

        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_mounts_jj_colocated_sets_git_host() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let repo_root = tmp.path().join("repo");
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        let mounts =
            resolve_mounts(
            "feat",
            &repo_root,
            &Vcs::Jj,
            vec![],
            None,
            None,
            false,
            "am",
            None,
        ).unwrap();

        assert_eq!(mounts.colocated_git_host, Some(repo_root.join(".git")));

        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_mounts_jj_non_colocated_no_git_host() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let repo_root = tmp.path().join("repo");
        let mounts =
            resolve_mounts(
            "feat",
            &repo_root,
            &Vcs::Jj,
            vec![],
            None,
            None,
            false,
            "am",
            None,
        ).unwrap();

        assert_eq!(mounts.colocated_git_host, None);

        std::env::remove_var("HOME");
    }

    #[test]
    fn build_run_command_publishes_forwarded_ports_on_loopback() {
        use crate::devcontainer::ForwardedPort;
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            ports: vec![
                ForwardedPort::Own(3000),
                // Names another compose service, so it has no meaning for a single container.
                ForwardedPort::Service { service: "db".into(), port: 5432 },
            ],
            ..DevcontainerRuntime::default()
        };
        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &make_mounts(tmp.path()),
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &dc,
        );
        let joined = cmd.join(" ");
        assert!(joined.contains("-p 127.0.0.1:3000:3000"), "got: {joined}");
        assert!(!joined.contains("5432"), "a service port cannot be published here: {joined}");
    }

    #[test]
    fn build_run_command_mounts_colocated_git_when_set() {
        let tmp = TempDir::new().unwrap();
        let main_git = tmp.path().join("main/.git");
        std::fs::create_dir_all(&main_git).unwrap();
        let mut mounts = make_mounts(tmp.path());
        mounts.colocated_git_host = Some(main_git.clone());

        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains(main_git.to_string_lossy().as_ref()),
            "expected colocated git mount, got: {joined}"
        );
    }

    #[test]
    fn resolve_mounts_includes_preflighted_agent_auth_for_claude() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::fs::create_dir(tmp.path().join(".claude")).unwrap();

        let agent_auth = preflight_agent_auth(KnownAgent::Claude, "/home/am").unwrap();
        let mounts = resolve_mounts(
            "feat",
            tmp.path(),
            &Vcs::Git,
            agent_auth.mounts,
            None,
            None,
            false,
            "am",
            None,
        )
        .unwrap();
        assert_eq!(mounts.agent_auth.len(), 2);
        assert_eq!(mounts.agent_auth[0].host_path, tmp.path().join(".claude"));
        assert_eq!(
            mounts.agent_auth[0].container_path,
            PathBuf::from("/home/am/.claude")
        );

        std::env::remove_var("HOME");
    }

    // ── SSH agent forwarding ──────────────────────────────────────────────────

    #[test]
    fn resolve_mounts_picks_up_the_host_ssh_agent_socket() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("SSH_AUTH_SOCK", "/run/user/1000/keyring/ssh");

        let mounts = resolve_mounts(
            "feat",
            &tmp.path().join("repo"),
            &Vcs::Git,
            vec![],
            None,
            None,
            true,
            "am",
            None,
        )
        .unwrap();

        assert_eq!(
            mounts.ssh_agent_sock,
            Some(PathBuf::from("/run/user/1000/keyring/ssh"))
        );

        std::env::remove_var("SSH_AUTH_SOCK");
        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_mounts_skips_the_ssh_agent_when_disabled() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("SSH_AUTH_SOCK", "/run/user/1000/keyring/ssh");

        let mounts = resolve_mounts(
            "feat",
            &tmp.path().join("repo"),
            &Vcs::Git,
            vec![],
            None,
            None,
            false,
            "am",
            None,
        )
        .unwrap();

        assert_eq!(mounts.ssh_agent_sock, None);

        std::env::remove_var("SSH_AUTH_SOCK");
        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_mounts_tolerates_a_host_with_no_agent() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        // Empty is what a shell leaves behind after `unset`-then-export patterns, and it
        // must read the same as absent — an empty mount source is a runtime error.
        std::env::set_var("SSH_AUTH_SOCK", "");

        let mounts = resolve_mounts(
            "feat",
            &tmp.path().join("repo"),
            &Vcs::Git,
            vec![],
            None,
            None,
            true,
            "am",
            None,
        )
        .unwrap();

        assert_eq!(mounts.ssh_agent_sock, None);

        std::env::remove_var("SSH_AUTH_SOCK");
        std::env::remove_var("HOME");
    }

    #[test]
    fn build_run_command_forwards_the_ssh_agent_at_the_host_path() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("agent.sock");
        std::fs::write(&sock, "").unwrap();
        let mut mounts = make_mounts(tmp.path());
        mounts.ssh_agent_sock = Some(sock.clone());

        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );

        let joined = cmd.join(" ");
        let sock = sock.to_string_lossy();
        // Same path on both sides, so SSH_AUTH_SOCK carries over unchanged.
        assert!(
            joined.contains(&format!("{sock}:{sock}:rw")),
            "expected a read-write mount at the host path, got: {joined}"
        );
        assert!(
            joined.contains(&format!("-e SSH_AUTH_SOCK={sock}")),
            "expected SSH_AUTH_SOCK to be set, got: {joined}"
        );
        // Podman on Linux relabels every other mount; this one must be left alone or the
        // host's own agent loses access to its socket.
        assert!(
            !joined.contains(&format!("{sock}:rw,z")),
            "the agent socket must not be relabelled, got: {joined}"
        );
    }

    #[test]
    fn build_run_command_skips_a_stale_ssh_agent_socket() {
        let tmp = TempDir::new().unwrap();
        let mut mounts = make_mounts(tmp.path());
        // A path left over from a dead agent. Forwarding it would set SSH_AUTH_SOCK to
        // something that cannot be connected to, which is worse than leaving it unset.
        mounts.ssh_agent_sock = Some(tmp.path().join("gone.sock"));

        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );

        assert!(
            !cmd.join(" ").contains("SSH_AUTH_SOCK"),
            "a missing socket must not be forwarded"
        );
    }

    // ── build_run_command ─────────────────────────────────────────────────────

    fn podman_runtime() -> ContainerRuntime {
        ContainerRuntime {
            kind: RuntimeKind::Podman,
            bin: PathBuf::from("/usr/bin/podman"),
        }
    }

    fn docker_runtime() -> ContainerRuntime {
        ContainerRuntime {
            kind: RuntimeKind::Docker,
            bin: PathBuf::from("/usr/bin/docker"),
        }
    }

    #[test]
    fn build_run_command_includes_required_flags() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let worktree = tmp
            .path()
            .join("worktrees/feat")
            .to_string_lossy()
            .into_owned();
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );

        let joined = cmd.join(" ");
        assert!(joined.contains("run"), "missing 'run'");
        assert!(joined.contains("--rm"));
        assert!(joined.contains("-it"));
        assert!(joined.contains("--name am-feat"));
        assert!(joined.contains(&worktree));
        assert!(joined.contains(&format!("--workdir {worktree}")));
        assert!(joined.contains("ubuntu:25.10"));
        assert!(!joined.contains("GIT_DIR"), "GIT_DIR should not be set");
        assert!(
            !joined.contains("GIT_WORK_TREE"),
            "GIT_WORK_TREE should not be set"
        );
    }

    #[test]
    fn build_run_command_includes_all_mounts() {
        let tmp = TempDir::new().unwrap();
        // Create the paths so the existence checks pass
        std::fs::write(tmp.path().join(".gitconfig"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join(".ssh")).unwrap();
        let mounts = make_mounts(tmp.path());
        let worktree = tmp
            .path()
            .join("worktrees/feat")
            .to_string_lossy()
            .into_owned();
        let git = tmp.path().join(".git").to_string_lossy().into_owned();
        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(joined.contains(&worktree), "missing worktree mount");
        assert!(joined.contains(&git), "missing vcs mount");
        assert!(joined.contains("/home/am/.gitconfig"));
        assert!(joined.contains("/home/am/.ssh"));
    }

    #[test]
    fn build_run_command_mounts_use_host_paths() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let worktree = tmp
            .path()
            .join("worktrees/feat")
            .to_string_lossy()
            .into_owned();
        let git = tmp.path().join(".git").to_string_lossy().into_owned();
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        // Container path should equal host path for worktree and vcs
        assert!(
            joined.contains(&format!("{worktree}:{worktree}")),
            "worktree mount should use host path: {joined}"
        );
        assert!(
            joined.contains(&format!("{git}:{git}")),
            "vcs mount should use host path: {joined}"
        );
        assert!(
            joined.contains(&format!("--workdir {worktree}")),
            "workdir should be worktree path: {joined}"
        );
    }

    #[test]
    fn build_run_command_selinux_z_on_linux_podman() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        // On Linux with Podman, all mounts should have ,z
        // On macOS they should not — test what the current platform does
        if cfg!(target_os = "linux") {
            assert!(
                joined.contains(",z"),
                "expected ,z on Linux+Podman, got: {joined}"
            );
        } else {
            assert!(
                !joined.contains(",z"),
                "unexpected ,z on non-Linux, got: {joined}"
            );
        }
    }

    #[test]
    fn build_run_command_no_selinux_z_for_docker() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(
            !joined.contains(",z"),
            "Docker should never have ,z: {joined}"
        );
    }

    // ── Devcontainer run path ─────────────────────────────────────────────────

    fn dc_run(dc: &DevcontainerRuntime, tmp: &Path) -> String {
        build_run_command(
            &podman_runtime(),
            "am-dc-abc",
            &make_mounts(tmp),
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            dc,
        )
        .join(" ")
    }

    #[test]
    fn devcontainer_env_becomes_e_flags() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            env: vec![("FOO".to_string(), "bar".to_string())],
            ..Default::default()
        };
        assert!(dc_run(&dc, tmp.path()).contains("-e FOO=bar"));
    }

    #[test]
    fn devcontainer_bind_mounts_get_am_selinux_labelling() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: Some("/var/run/docker.sock".to_string()),
                target: "/var/run/docker-host.sock".to_string(),
                kind: "bind".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        let joined = dc_run(&dc, tmp.path());
        assert!(joined.contains("/var/run/docker.sock:/var/run/docker-host.sock:rw"));
        #[cfg(target_os = "linux")]
        assert!(joined.contains("/var/run/docker-host.sock:rw,z"));
    }

    #[test]
    fn devcontainer_volume_mounts_are_not_relabelled() {
        // `,z` on a named volume is meaningless and podman rejects some combinations.
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: Some("my-volume".to_string()),
                target: "/data".to_string(),
                kind: "volume".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        let joined = dc_run(&dc, tmp.path());
        assert!(joined.contains("my-volume:/data:rw"));
        assert!(!joined.contains("my-volume:/data:rw,z"));
    }

    #[test]
    fn devcontainer_read_only_mount_is_marked_ro() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: Some("/host".to_string()),
                target: "/ro".to_string(),
                kind: "bind".to_string(),
                read_only: true,
            }],
            ..Default::default()
        };
        assert!(dc_run(&dc, tmp.path()).contains("/host:/ro:ro"));
    }

    #[test]
    fn devcontainer_mount_without_a_source_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: None,
                target: "/nowhere".to_string(),
                kind: "bind".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        assert!(!dc_run(&dc, tmp.path()).contains("/nowhere"));
    }

    #[test]
    fn devcontainer_escalating_options_are_applied_when_granted() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            init: true,
            privileged: true,
            cap_add: vec!["SYS_PTRACE".to_string()],
            security_opt: vec!["label=disable".to_string()],
            ..Default::default()
        };
        let joined = dc_run(&dc, tmp.path());
        assert!(joined.contains("--init"));
        assert!(joined.contains("--privileged"));
        assert!(joined.contains("--cap-add SYS_PTRACE"));
        assert!(joined.contains("--security-opt label=disable"));
    }

    #[test]
    fn escalating_options_are_absent_by_default() {
        let tmp = TempDir::new().unwrap();
        let joined = dc_run(&DevcontainerRuntime::default(), tmp.path());
        assert!(!joined.contains("--privileged"));
        assert!(!joined.contains("--cap-add"));
        assert!(!joined.contains("--init"));
    }

    #[test]
    fn workspace_folder_overrides_the_mirrored_workdir() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            workdir: Some("/workspaces/custom".to_string()),
            ..Default::default()
        };
        assert!(dc_run(&dc, tmp.path()).contains("--workdir /workspaces/custom"));
    }

    #[test]
    fn am_network_setting_wins_over_run_args() {
        // A config that asks for --network=host must not defeat container.network = "none".
        let tmp = TempDir::new().unwrap();
        let cmd = build_run_command(
            &podman_runtime(),
            "am-dc-abc",
            &make_mounts(tmp.path()),
            &[],
            &[],
            &NetworkMode::None,
            "am-feat",
            &DevcontainerRuntime {
                run_args: vec!["--network=host".to_string()],
                ..Default::default()
            },
        );
        let host = cmd.iter().position(|a| a == "--network=host").unwrap();
        let none = cmd.iter().position(|a| a == "--network").unwrap();
        assert!(none > host, "am's --network none must come last");
    }

    #[test]
    fn devcontainer_remote_user_becomes_the_container_user() {
        // Without this the image's default user (root) wins and $HOME is /root, so the
        // credentials am mounts under the remoteUser's home are never found.
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            user: Some("vscode".to_string()),
            ..Default::default()
        };
        assert!(dc_run(&dc, tmp.path()).contains("--user vscode"));
    }

    #[test]
    fn image_mode_keeps_the_numeric_user_mapping() {
        let tmp = TempDir::new().unwrap();
        let joined = dc_run(&DevcontainerRuntime::default(), tmp.path());
        assert!(!joined.contains("--user vscode"));
    }

    #[test]
    fn docker_prefers_the_named_user_over_the_numeric_mapping() {
        // Two --user flags would leave docker using the last one; emit only one.
        let tmp = TempDir::new().unwrap();
        let cmd = build_run_command(
            &ContainerRuntime {
                kind: RuntimeKind::Docker,
                bin: PathBuf::from("/usr/bin/docker"),
            },
            "am-dc-abc",
            &make_mounts(tmp.path()),
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime {
                user: Some("vscode".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(cmd.iter().filter(|a| *a == "--user").count(), 1);
        assert!(cmd.contains(&"vscode".to_string()));
    }

    #[test]
    fn container_home_uses_root_for_the_root_user() {
        // /home/root does not exist; credentials mounted there are silently invisible.
        assert_eq!(container_home("root", None), "/root");
        assert_eq!(container_home("vscode", None), "/home/vscode");
        assert_eq!(
            container_home("vscode", Some(Path::new("/custom"))),
            "/custom"
        );
    }

    #[test]
    fn gitconfig_and_ssh_follow_the_derived_home() {
        let tmp = TempDir::new().unwrap();
        let mut mounts = make_mounts(tmp.path());
        mounts.container_home = container_home("root", None);
        std::fs::write(&mounts.gitconfig_host, "").unwrap();
        std::fs::create_dir_all(&mounts.ssh_host).unwrap();
        let joined = build_run_command(
            &podman_runtime(),
            "am-dc-abc",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        )
        .join(" ");
        assert!(joined.contains("/root/.gitconfig"));
        assert!(joined.contains("/root/.ssh"));
    }

    // ── Entrypoint composition ────────────────────────────────────────────────

    use crate::devcontainer::UserEnvProbe;

    /// The common case in these tests: no userEnvProbe, so the script is only the chain.
    fn compose_entrypoint_command_no_probe(
        entrypoints: &[String],
        agent_cmd: &[String],
    ) -> Vec<String> {
        compose_entrypoint_command(entrypoints, agent_cmd, UserEnvProbe::None, &[])
    }

    #[test]
    fn no_entrypoints_leaves_the_agent_command_alone() {
        let agent = vec!["claude".to_string()];
        assert_eq!(compose_entrypoint_command(&[], &agent, UserEnvProbe::None, &[]), agent);
    }

    #[test]
    fn entrypoints_are_chained_before_the_agent() {
        let cmd = compose_entrypoint_command_no_probe(
            &[
                "/usr/local/share/docker-init.sh".to_string(),
                "/usr/local/share/ssh-init.sh".to_string(),
            ],
            &["claude".to_string()],
        );
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
        assert_eq!(
            cmd[2],
            "/usr/local/share/docker-init.sh && /usr/local/share/ssh-init.sh && exec claude"
        );
    }

    #[test]
    fn entrypoints_run_even_without_an_agent_command() {
        let cmd = compose_entrypoint_command_no_probe(&["/init.sh".to_string()], &[]);
        assert_eq!(cmd[2], "/init.sh");
    }

    #[test]
    fn a_probe_runs_before_the_agent_and_does_not_gate_it() {
        let cmd = compose_entrypoint_command(
            &[],
            &["claude".to_string()],
            UserEnvProbe::LoginInteractiveShell,
            &[],
        );
        let script = &cmd[2];
        assert!(script.contains("-lic 'cat /proc/self/environ'"), "got: {script}");
        // Newline, not `&&`: a probe that finds nothing is not a failure, and must not swallow
        // the agent the way a failed entrypoint deliberately does.
        assert!(script.contains("\nexec claude"), "got: {script}");
        assert!(!script.contains("&& exec claude"), "got: {script}");
    }

    #[test]
    fn a_failing_entrypoint_still_gates_the_agent_when_probing() {
        let cmd = compose_entrypoint_command(
            &["/init.sh".to_string()],
            &["claude".to_string()],
            UserEnvProbe::LoginShell,
            &[],
        );
        assert!(cmd[2].contains("/init.sh && exec claude"), "got: {}", cmd[2]);
    }

    #[test]
    fn each_probe_mode_maps_to_the_shell_flags_the_cli_uses() {
        let flags = |p: UserEnvProbe| {
            compose_entrypoint_command(&[], &["a".to_string()], p, &[]).join(" ")
        };
        assert!(flags(UserEnvProbe::LoginInteractiveShell).contains(" -lic "));
        assert!(flags(UserEnvProbe::LoginShell).contains(" -lc "));
        assert!(flags(UserEnvProbe::InteractiveShell).contains(" -ic "));
        // `none` must leave the command completely alone — no shell wrapper at all.
        assert_eq!(
            compose_entrypoint_command(&[], &["a".to_string()], UserEnvProbe::None, &[]),
            vec!["a".to_string()]
        );
    }

    /// The generated snippet is shell, so the only real check is running it.
    #[cfg(unix)]
    #[test]
    fn the_probe_script_applies_what_it_finds_and_respects_protected_names() {
        let cmd = compose_entrypoint_command(
            &[],
            &["env".to_string()],
            UserEnvProbe::LoginShell,
            &["AM_KEEP".to_string()],
        );
        // Stand in for the container's login shell: it "sources a dotfile" that sets one new
        // variable and tries to clobber one am set on purpose.
        let tmp = TempDir::new().unwrap();
        let fake = tmp.path().join("fakeshell");
        std::fs::write(
            &fake,
            "#!/bin/sh\nAM_FROM_DOTFILE=yes AM_KEEP=clobbered /usr/bin/env -0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        // `getent` resolves the real user's shell, so point the script at the fake one instead.
        let script = cmd[2].replace(
            "_am_shell=$(getent passwd \"$(id -u)\" 2>/dev/null | cut -d: -f7)",
            &format!("_am_shell={}", fake.display()),
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env("AM_KEEP", "original")
            .output()
            .expect("running the generated script");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let env = String::from_utf8_lossy(&out.stdout);

        assert!(env.contains("AM_FROM_DOTFILE=yes"), "probed variable missing: {env}");
        assert!(
            env.contains("AM_KEEP=original"),
            "the probe overwrote a variable am set deliberately: {env}"
        );
    }

    #[test]
    fn protected_names_cover_every_source_the_run_command_sets() {
        let tmp = TempDir::new().unwrap();
        let mut mounts = make_mounts(tmp.path());
        std::fs::write(&mounts.gitconfig_host, "[user]\n\tname = T\n\temail = t@e.com\n").unwrap();
        let sock = tmp.path().join("agent.sock");
        std::fs::write(&sock, "").unwrap();
        mounts.ssh_agent_sock = Some(sock);

        let dc = DevcontainerRuntime {
            env: vec![("FROM_CONFIG".to_string(), "1".to_string())],
            ..DevcontainerRuntime::default()
        };
        let names = protected_env_names(
            &mounts,
            &["PASSED_THROUGH".to_string(), "WITH=value".to_string()],
            &[("AGENT_TOKEN".to_string(), "x".to_string())],
            &dc,
        );
        for expected in [
            "SSH_AUTH_SOCK",
            "JJ_USER",
            "JJ_EMAIL",
            "AGENT_TOKEN",
            "FROM_CONFIG",
            "PASSED_THROUGH",
            "WITH",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
        }
    }

    #[test]
    fn agent_flags_are_quoted_in_the_composed_script() {
        let cmd = compose_entrypoint_command_no_probe(
            &["/init.sh".to_string()],
            &[
                "claude".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "it's".to_string(),
            ],
        );
        assert!(cmd[2].contains("exec claude --dangerously-skip-permissions 'it'\\''s'"));
    }

    // ── jj identity ───────────────────────────────────────────────────────────

    fn write_gitconfig(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn jj_identity_env_derives_both_values_from_gitconfig() {
        let tmp = TempDir::new().unwrap();
        let gitconfig = tmp.path().join(".gitconfig");
        write_gitconfig(
            &gitconfig,
            "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
        );
        assert_eq!(
            jj_identity_env(&gitconfig),
            vec![
                ("JJ_USER".to_string(), "Ada Lovelace".to_string()),
                ("JJ_EMAIL".to_string(), "ada@example.com".to_string()),
            ]
        );
    }

    #[test]
    fn jj_identity_env_empty_when_gitconfig_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(jj_identity_env(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn jj_identity_env_empty_when_identity_incomplete() {
        let tmp = TempDir::new().unwrap();
        // A name with no email is worse than nothing: jj would still refuse to push,
        // but the commit would look configured.
        let gitconfig = tmp.path().join(".gitconfig");
        write_gitconfig(&gitconfig, "[user]\n\tname = Ada Lovelace\n");
        assert!(jj_identity_env(&gitconfig).is_empty());

        write_gitconfig(&gitconfig, "[core]\n\tautocrlf = false\n");
        assert!(jj_identity_env(&gitconfig).is_empty());
    }

    #[test]
    fn build_run_command_injects_jj_identity() {
        let tmp = TempDir::new().unwrap();
        write_gitconfig(
            &tmp.path().join(".gitconfig"),
            "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
        );
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        assert!(cmd.contains(&"JJ_USER=Ada Lovelace".to_string()));
        assert!(cmd.contains(&"JJ_EMAIL=ada@example.com".to_string()));
    }

    #[test]
    fn build_run_command_jj_identity_yields_to_explicit_env() {
        let tmp = TempDir::new().unwrap();
        write_gitconfig(
            &tmp.path().join(".gitconfig"),
            "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n",
        );
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[("JJ_EMAIL".to_string(), "override@example.com".to_string())],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let derived = cmd
            .iter()
            .position(|a| a == "JJ_EMAIL=ada@example.com")
            .expect("derived identity missing");
        let explicit = cmd
            .iter()
            .position(|a| a == "JJ_EMAIL=override@example.com")
            .expect("explicit override missing");
        assert!(
            derived < explicit,
            "derived identity must come first so the later -e wins"
        );
    }

    #[test]
    fn build_run_command_omits_jj_identity_without_gitconfig() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        assert!(!cmd.iter().any(|a| a.starts_with("JJ_USER=")));
        assert!(!cmd.iter().any(|a| a.starts_with("JJ_EMAIL=")));
    }

    #[test]
    fn image_mode_runs_an_init_process() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::image_mode(),
        );
        assert!(cmd.contains(&"--init".to_string()));
    }

    #[test]
    fn devcontainer_mode_leaves_init_to_the_config() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let without = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        assert!(!without.contains(&"--init".to_string()));

        let with = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime {
                init: true,
                ..DevcontainerRuntime::default()
            },
        );
        assert!(with.contains(&"--init".to_string()));
    }

    #[test]
    fn build_run_command_network_none() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::None,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        assert!(cmd.contains(&"--network".to_string()));
        assert!(cmd.contains(&"none".to_string()));
    }

    #[test]
    fn build_run_command_env_passthrough() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &["ANTHROPIC_API_KEY".to_string()],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(joined.contains("ANTHROPIC_API_KEY"));
    }

    // ── stop / remove ─────────────────────────────────────────────────────────

    #[test]
    fn stop_container_sends_stop_command() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("log");
        std::env::set_var("MOCK_CONTAINER_LOG", &log);
        let rt = fake_runtime(RuntimeKind::Podman, tmp.path());

        stop_container(&rt, "am-feat").unwrap();

        let out = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(out.contains("stop"), "expected 'stop', got: {out}");
        assert!(out.contains("am-feat"));

        std::env::remove_var("MOCK_CONTAINER_LOG");
    }

    #[test]
    fn remove_container_sends_rm_command() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("log");
        std::env::set_var("MOCK_CONTAINER_LOG", &log);
        let rt = fake_runtime(RuntimeKind::Podman, tmp.path());

        remove_container(&rt, "am-feat").unwrap();

        let out = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(out.contains("rm"), "expected 'rm', got: {out}");
        assert!(out.contains("-f"));
        assert!(out.contains("am-feat"));

        std::env::remove_var("MOCK_CONTAINER_LOG");
    }

    // ── Feature 4: Claude auth resolution ─────────────────────────────────────

    #[test]
    fn resolve_agent_auth_claude_defaults_to_dot_claude() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let mounts = resolve_agent_auth_mounts(KnownAgent::Claude, "/home/am").unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, tmp.path().join(".claude"));
        assert_eq!(mounts[0].container_path, PathBuf::from("/home/am/.claude"));
        assert_eq!(mounts[0].mode, MountMode::ReadWrite);
        assert_eq!(mounts[1].host_path, tmp.path().join(".claude.json"));
        assert_eq!(
            mounts[1].container_path,
            PathBuf::from("/home/am/.claude.json")
        );
        assert_eq!(mounts[1].mode, MountMode::ReadWrite);

        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_agent_auth_claude_uses_claude_config_dir_when_set() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let custom_config = tmp.path().join("custom-claude-config");
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("CLAUDE_CONFIG_DIR", &custom_config);

        let mounts = resolve_agent_auth_mounts(KnownAgent::Claude, "/home/am").unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, custom_config);
        assert_eq!(mounts[0].container_path, PathBuf::from("/home/am/.claude"));
        assert_eq!(mounts[0].mode, MountMode::ReadWrite);
        assert_eq!(mounts[1].host_path, tmp.path().join(".claude.json"));
        assert_eq!(
            mounts[1].container_path,
            PathBuf::from("/home/am/.claude.json")
        );

        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("HOME");
    }

    #[test]
    fn build_run_command_includes_claude_mount_when_active() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        // Create the claude config dir so the existence check passes
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();

        let mut mounts = make_mounts(tmp.path());
        mounts.agent_auth = vec![AgentAuthMount {
            host_path: tmp.path().join(".claude"),
            container_path: PathBuf::from("/home/am/.claude"),
            mode: MountMode::ReadWrite,
        }];

        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains("/home/am/.claude"),
            "expected claude mount, got: {joined}"
        );

        std::env::remove_var("HOME");
    }

    // ── KnownAgent::parse ─────────────────────────────────────────────

    #[test]
    fn known_agent_parse_known_agents_ok() {
        assert!(KnownAgent::parse("claude").is_ok());
        assert!(KnownAgent::parse("copilot").is_ok());
        assert!(KnownAgent::parse("gemini").is_ok());
        assert!(KnownAgent::parse("codex").is_ok());
    }

    #[test]
    fn known_agent_parse_unknown_errors() {
        let err = KnownAgent::parse("my-custom-agent").unwrap_err();
        assert!(err.to_string().contains("my-custom-agent"));
    }

    #[test]
    fn known_agent_display_matches_parse_input() {
        for agent in [
            KnownAgent::Claude,
            KnownAgent::Copilot,
            KnownAgent::Gemini,
            KnownAgent::Codex,
        ] {
            let s = agent.to_string();
            assert_eq!(KnownAgent::parse(&s).unwrap(), agent);
        }
    }

    // ── preflight_agent_auth ───────────────────────────────────────────

    #[test]
    fn preflight_agent_auth_claude_ok_when_dir_exists() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::fs::create_dir(tmp.path().join(".claude")).unwrap();

        assert!(preflight_agent_auth(KnownAgent::Claude, "/home/am").is_ok());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_claude_fails_when_dir_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert!(preflight_agent_auth(KnownAgent::Claude, "/home/am").is_err());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_copilot_returns_mounts_and_env() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        let gh = fake_gh(tmp.path(), "echo gh-test-token");
        std::env::set_var("AM_GH_BIN", &gh);
        std::fs::create_dir_all(tmp.path().join(".config").join("gh")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".config").join("github-copilot")).unwrap();

        let auth = preflight_agent_auth(KnownAgent::Copilot, "/home/am").unwrap();
        assert_eq!(
            auth.env,
            vec![("GH_TOKEN".to_string(), "gh-test-token".to_string())]
        );
        assert_eq!(auth.mounts.len(), 2);

        std::env::remove_var("AM_GH_BIN");
        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_copilot_fails_when_gh_dir_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        assert!(preflight_agent_auth(KnownAgent::Copilot, "/home/am").is_err());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_gemini_ok_when_dir_exists() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::fs::create_dir(tmp.path().join(".gemini")).unwrap();

        assert!(preflight_agent_auth(KnownAgent::Gemini, "/home/am").is_ok());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_gemini_fails_when_dir_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        assert!(preflight_agent_auth(KnownAgent::Gemini, "/home/am").is_err());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_codex_ok_when_key_set() {
        let _g = lock_env();
        // A temp HOME with no ~/.codex: the API-key-only user, and hermetic against
        // whatever HOME a neighbouring test left behind.
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("OPENAI_API_KEY", "sk-test");

        let auth = preflight_agent_auth(KnownAgent::Codex, "/home/am").unwrap();
        assert_eq!(
            auth.env,
            vec![("OPENAI_API_KEY".to_string(), "sk-test".to_string())]
        );
        assert!(auth.mounts.is_empty());

        std::env::remove_var("OPENAI_API_KEY");
    }

    // These pin the no-credentials path, so HOME must point somewhere without a
    // ~/.codex — otherwise the result depends on whether the developer running the
    // suite happens to have signed into codex.
    #[test]
    fn preflight_agent_auth_codex_fails_when_key_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("OPENAI_API_KEY");

        let err = preflight_agent_auth(KnownAgent::Codex, "/home/am").unwrap_err();
        assert!(err.to_string().contains("OPENAI_API_KEY"));

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_codex_fails_when_key_empty() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("OPENAI_API_KEY", "");

        let err = preflight_agent_auth(KnownAgent::Codex, "/home/am").unwrap_err();
        assert!(err.to_string().contains("OPENAI_API_KEY"));

        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_codex_accepts_an_interactive_signin() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::write(tmp.path().join(".codex").join("auth.json"), "{}").unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("OPENAI_API_KEY");

        // No API key anywhere, and this must still be enough: it is how codex works
        // for anyone who signed in rather than exporting a key.
        let auth = preflight_agent_auth(KnownAgent::Codex, "/home/am").unwrap();
        assert!(auth.env.is_empty());
        assert_eq!(auth.mounts.len(), 1);

        std::env::remove_var("HOME");
    }

    #[test]
    fn credentials_hint_names_a_concrete_command_per_agent() {
        assert!(credentials_hint(KnownAgent::Claude).contains("claude auth login"));
        assert!(credentials_hint(KnownAgent::Claude).contains("ANTHROPIC_API_KEY"));
        assert!(credentials_hint(KnownAgent::Claude)
            .contains("https://dstanek.github.io/agent-manager/guides/claude-code/#prerequisites"));

        assert!(credentials_hint(KnownAgent::Copilot).contains("gh auth login"));
        assert!(credentials_hint(KnownAgent::Copilot).contains(
            "https://dstanek.github.io/agent-manager/guides/github-copilot/#prerequisites"
        ));

        assert!(credentials_hint(KnownAgent::Gemini).contains("Gemini CLI"));
        assert!(credentials_hint(KnownAgent::Gemini)
            .contains("https://dstanek.github.io/agent-manager/guides/gemini/#prerequisites"));

        assert!(credentials_hint(KnownAgent::Codex).contains("codex"));
        assert!(credentials_hint(KnownAgent::Codex).contains("OPENAI_API_KEY"));
        assert!(credentials_hint(KnownAgent::Codex)
            .contains("https://dstanek.github.io/agent-manager/guides/codex/#prerequisites"));
    }

    #[test]
    fn codex_mounts_the_config_dir_read_write_when_present() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::env::set_var("HOME", tmp.path());

        let mounts = resolve_agent_auth_mounts(KnownAgent::Codex, "/home/am").unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host_path, tmp.path().join(".codex"));
        assert_eq!(mounts[0].container_path, PathBuf::from("/home/am/.codex"));
        // Read-write: codex rotates the token in auth.json, and a read-only mount
        // would work until the first refresh and then break.
        assert_eq!(mounts[0].mode, MountMode::ReadWrite);

        std::env::remove_var("HOME");
    }

    #[test]
    fn build_run_command_includes_codex_api_key_env() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let cmd = build_run_command(
            &podman_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[("OPENAI_API_KEY".to_string(), "sk-test-key".to_string())],
            &NetworkMode::Full,
            "am-feat",
            &DevcontainerRuntime::default(),
        );
        let joined = cmd.join(" ");
        assert!(joined.contains("-e OPENAI_API_KEY=sk-test-key"));
    }

    // ── Feature 6: agent auth resolution ──────────────────────────────────────

    #[test]
    fn resolve_agent_auth_copilot_returns_both_dirs() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let auth_mounts = resolve_agent_auth_mounts(KnownAgent::Copilot, "/home/am").unwrap();
        assert_eq!(auth_mounts.len(), 2);

        let paths: Vec<_> = auth_mounts.iter().map(|m| m.host_path.clone()).collect();
        assert!(
            paths.contains(&tmp.path().join(".config").join("gh")),
            "missing gh config"
        );
        assert!(
            paths.contains(&tmp.path().join(".config").join("github-copilot")),
            "missing github-copilot config"
        );

        for m in &auth_mounts {
            assert_eq!(
                m.mode,
                MountMode::ReadOnly,
                "copilot mounts should be read-only"
            );
        }

        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_agent_auth_copilot_container_paths_match() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let auth_mounts = resolve_agent_auth_mounts(KnownAgent::Copilot, "/home/am").unwrap();
        let container_paths: Vec<_> = auth_mounts
            .iter()
            .map(|m| m.container_path.clone())
            .collect();
        assert!(container_paths.contains(&PathBuf::from("/home/am/.config/gh")));
        assert!(container_paths.contains(&PathBuf::from("/home/am/.config/github-copilot")));

        std::env::remove_var("HOME");
    }

    #[test]
    fn resolve_agent_auth_gemini_returns_dot_gemini() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        let auth_mounts = resolve_agent_auth_mounts(KnownAgent::Gemini, "/home/am").unwrap();
        assert_eq!(auth_mounts.len(), 1);
        assert_eq!(auth_mounts[0].host_path, tmp.path().join(".gemini"));
        assert_eq!(
            auth_mounts[0].container_path,
            PathBuf::from("/home/am/.gemini")
        );
        assert_eq!(auth_mounts[0].mode, MountMode::ReadOnly);

        std::env::remove_var("HOME");
    }

    #[test]
    fn codex_returns_no_mount_when_never_signed_in() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        // An API-key user may have no ~/.codex at all. Mounting a missing directory
        // would have the runtime create it, owned by root.
        assert!(resolve_agent_auth_mounts(KnownAgent::Codex, "/home/am")
            .unwrap()
            .is_empty());

        std::env::remove_var("HOME");
    }
}
