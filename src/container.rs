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
    /// The user to drop to for the agent, when the container itself must start as someone
    /// else. Set only when a Feature contributes an entrypoint: those run as the *container*
    /// user — root unless the config says otherwise — while the agent and the lifecycle hooks
    /// run as `remoteUser`. Verified against the reference CLI, which starts the container as
    /// root and `exec`s tools as the remote user.
    pub drop_to: Option<String>,
    /// Whether to map the container user onto the host's UID/GID, from `updateRemoteUserUID`.
    pub update_remote_user_uid: bool,
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

/// Where an agent's credentials land inside the container.
///
/// `home_in_container` is passed in rather than derived from the username: a devcontainer's
/// `remoteUser` may be `root` (home `/root`, not `/home/root`), and mounting credentials at
/// a path the agent never reads fails silently at the worst moment.
fn resolve_credential_mounts(
    harness: &crate::harness::Harness,
    home_in_container: &str,
) -> Result<Vec<AgentAuthMount>> {
    let Some(integration) = harness.integration.as_ref() else {
        return Ok(vec![]);
    };
    let mut mounts = Vec::new();
    for spec in &integration.mounts {
        let host_path = match spec.host.resolve() {
            Ok(path) => path,
            // An agent that can authenticate without touching the filesystem must not fail
            // the whole preflight just because HOME is unresolvable.
            Err(_) if integration.home_optional => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        // Mounting a path that does not exist would have the runtime create it on the host,
        // root-owned.
        if spec.only_if_exists && !host_path.exists() {
            continue;
        }
        let container_path = if spec.container.starts_with('/') {
            PathBuf::from(&spec.container)
        } else {
            PathBuf::from(format!("{home_in_container}/{}", spec.container))
        };
        mounts.push(AgentAuthMount {
            host_path,
            container_path,
            mode: spec.mode.clone(),
        });
    }
    Ok(mounts)
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

/// The error for an agent whose credential requirements are not met.
///
/// Always returns `Err`. Two shapes, because two situations deserve different wording: with
/// a single requirement group there is exactly one missing path worth naming, and with
/// alternatives there is not — naming just one would send the user to fix the half they were
/// not using.
fn unsatisfied(
    harness: &crate::harness::Harness,
    integration: &crate::harness::Integration,
) -> Result<()> {
    let name = &harness.name;
    if let Some(message) = &integration.alternatives_message {
        return Err(anyhow::anyhow!("agent '{name}' has no credentials: {message}"));
    }
    match integration.unsatisfied_path() {
        Some(path) => Err(anyhow::anyhow!(
            "agent '{name}' requires path to exist: {}\n\
             Make sure {name} is installed and authenticated on this system",
            path.display()
        ))
        .with_context(|| {
            format!("checking agent credentials for '{name}' at {}", path.display())
        }),
        None => Err(anyhow::anyhow!("agent '{name}' has no credentials")),
    }
}

/// Resolve and validate an agent's authentication requirements before the container is
/// launched. This performs all preflight checks and returns the mounts and environment
/// variables needed for the actual runtime command.
pub fn preflight_agent_auth(
    harness: &crate::harness::Harness,
    home_in_container: &str,
) -> Result<AgentAuth> {
    let Some(integration) = harness.integration.as_ref() else {
        return Ok(AgentAuth::default());
    };
    let mounts = resolve_credential_mounts(harness, home_in_container)?;

    // Required mounts must exist before anything is built. The rest are best-effort.
    for spec in integration.mounts.iter().filter(|spec| spec.required) {
        let path = spec.host.resolve()?;
        if !path.exists() {
            unsatisfied(harness, integration)?;
        }
    }

    let mut env = Vec::new();
    for source in &integration.env {
        match source {
            crate::harness::EnvSource::Passthrough(var) => {
                if let Some(value) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
                    env.push((var.clone(), value));
                }
            }
            crate::harness::EnvSource::GhToken => {
                env.push(("GH_TOKEN".to_string(), get_gh_token()?))
            }
        }
    }

    // Nothing resolved at all. Only reachable for an agent whose every credential is
    // optional — one authenticated by alternatives, where each alternative is absent.
    if mounts.is_empty() && env.is_empty() {
        unsatisfied(harness, integration)?;
    }

    Ok(AgentAuth { mounts, env })
}

/// Check that an agent's credentials exist on the host, without deciding where they will be
/// mounted.
///
/// This exists so credential problems surface *before* `am start` creates a worktree. The
/// mount targets cannot be known that early in devcontainer mode — they depend on the
/// `remoteUser` recorded in an image that has not been built yet — but whether the user is
/// logged in does not depend on any of that.
///
/// Presence only. A revoked or expired credential leaves its path in place and passes here;
/// see `BACKLOG.md`.
pub fn validate_agent_credentials(harness: &crate::harness::Harness) -> Result<()> {
    let Some(integration) = harness.integration.as_ref() else {
        return Ok(());
    };
    if integration.satisfied() {
        return Ok(());
    }
    unsatisfied(harness, integration)
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
pub fn credentials_hint(harness: &crate::harness::Harness) -> &str {
    harness
        .integration
        .as_ref()
        .map(|integration| integration.hint.as_str())
        .unwrap_or("")
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

// Visibility must match the `cfg(unix)` arm above: `compose.rs` calls this, and a private
// fallback compiles everywhere it is never selected — which is to say, everywhere the developer
// who changed it was looking.
#[cfg(not(unix))]
pub(crate) fn get_host_uid_gid() -> Option<(u32, u32)> {
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
                // Docker has no `keep-id`, so the only way to make a bind-mounted worktree
                // writable is to run as the host's own uid. This used to be skipped whenever
                // the config named a user, on the assumption that a devcontainer user is
                // uid 1000 and therefore the same mapping by another name — true only for a
                // host user who is also 1000. For anyone else the container could not write
                // its own worktree.
                //
                // `updateRemoteUserUID` is the spec's switch for this, and defaults to true.
                // `am` maps the process rather than rewriting the image's passwd entry as the
                // reference CLI does, so `HOME` is set explicitly below: a numeric uid with no
                // passwd entry would otherwise leave the agent without one.
                // Not when privileges are about to be dropped: the container has to start as
                // the container user for the Feature entrypoints, and `su` from a mapped
                // numeric uid is not possible. The agent then runs as the image's remote user,
                // whose uid may differ from the host's — the reference CLI rewrites the image
                // to reconcile that, which `am` does not do.
                if dc.drop_to.is_none() && (dc.user.is_none() || dc.update_remote_user_uid) {
                    cmd.push("--user".to_string());
                    cmd.push(format!("{uid}:{gid}"));
                }
            }
        }
    }

    // Run as the devcontainer's remoteUser rather than the image's default (usually root),
    // so $HOME matches where the credential mounts land. Skipped when the uid mapping above
    // already applies, since the two `--user` flags would contradict each other and the
    // numeric one is what keeps the worktree writable.
    let mapped_uid = matches!(runtime.kind, RuntimeKind::Docker)
        && dc.drop_to.is_none()
        && dc.update_remote_user_uid
        && get_host_uid_gid().is_some();
    if let Some(ref user) = dc.user {
        if !mapped_uid {
            cmd.push("--user".to_string());
            cmd.push(user.clone());
        } else {
            // Running as a bare uid means no passwd entry, so nothing derives HOME. The
            // credential mounts are already placed at this path.
            cmd.push("-e".to_string());
            cmd.push(format!("HOME={home}"));
        }
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
        if let Some(spec) = port.spec() {
            cmd.push("-p".to_string());
            cmd.push(spec);
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

/// Drop published ports whose host binding is already taken, returning what was dropped.
///
/// Two sessions on the same repo forward the same ports, because they come from the same
/// `devcontainer.json` — so the second one hits `bind: address already in use`. Without this the
/// failure arrives from `podman run`, after the worktree, the image and the tmux window are all
/// built, and takes the whole session down: a config's *convenience* property stops the agent
/// from starting at all. Dropping the port and saying so keeps the session, which is what the
/// user asked for; the port is still reachable in the first session, which is where whatever is
/// listening on it actually runs.
///
/// A spec with no host port (`"8080"` alone, from `appPort`) asks the runtime to pick a free one,
/// so there is nothing to check and it is kept as-is.
pub fn drop_busy_ports(ports: &mut Vec<crate::devcontainer::ForwardedPort>) -> Vec<u16> {
    let mut dropped = Vec::new();
    ports.retain(|port| {
        let Some(spec) = port.spec() else { return true };
        let Some((addr, number)) = host_binding(&spec) else { return true };
        // Only a genuine collision drops the port. Any other failure — an address this host
        // cannot bind, a permission error — is something we cannot interpret, and the runtime
        // deserves the chance to try it rather than am second-guessing the config.
        match std::net::TcpListener::bind((addr.as_str(), number)) {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                dropped.push(number);
                false
            }
            _ => true,
        }
    });
    dropped
}

/// The host address and port a publish spec binds, or `None` when the runtime picks the port.
///
/// `-p` takes `[ip:][hostPort:]containerPort`. Only the host half matters here.
fn host_binding(spec: &str) -> Option<(String, u16)> {
    let parts: Vec<&str> = spec.rsplitn(3, ':').collect();
    match parts.as_slice() {
        // "containerPort" — the runtime assigns the host port, so nothing can collide.
        [_] => None,
        [_, host] => Some(("0.0.0.0".to_string(), host.parse().ok()?)),
        [_, host, ip] => Some((ip.to_string(), host.parse().ok()?)),
        _ => None,
    }
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
    hooks: &[String],
    agent_cmd: &[String],
    probe: crate::devcontainer::UserEnvProbe,
    protected: &[String],
    drop_to: Option<&str>,
) -> Vec<String> {
    let probe_script = user_env_probe_script(probe, protected);
    if entrypoints.is_empty() && hooks.is_empty() && probe_script.is_none() {
        return agent_cmd.to_vec();
    }

    // What the *remote* user runs: probe the environment, run the lifecycle hooks, exec the
    // agent. Hooks stay `&&`-chained through to the agent — a failed postCreateCommand must
    // stop the session rather than launch an agent into a half-built container — while the
    // probe is joined with a newline, because finding no variables is not a failure.
    let mut tail = hooks.join(" && ");
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
    let user_script = match probe_script {
        Some(probe) if tail.is_empty() => probe,
        Some(probe) => format!("{probe}\n{tail}"),
        None => tail,
    };

    // What the *container* user runs: the Feature entrypoints, then a drop to the remote user
    // for everything above. `su` rather than a second `--user`, because there is only one
    // command — and `exec` so the agent keeps the pane's tty and PID 1's signals.
    // The entrypoints stay `&&`-chained so one failing stops the session. The UID alignment is
    // a separate statement on its own line: it is best-effort, and chaining it would either
    // gate the entrypoints on it or rely on `||`/`&&` precedence to avoid doing so.
    let mut script = entrypoints.join(" && ");
    if let Some(align) = drop_to.and_then(align_uid_script) {
        script = if script.is_empty() { align } else { format!("{align}\n{script}") };
    }

    let dropped = match drop_to {
        Some(user) if !user_script.is_empty() => {
            format!("exec su {} -s /bin/sh -c {}", shell_quote(user), shell_quote(&user_script))
        }
        _ => user_script,
    };
    if !dropped.is_empty() {
        if script.is_empty() {
            script = dropped;
        } else {
            script.push_str(&format!(" && {dropped}"));
        }
    }
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

/// Give the user we are about to drop to the host's UID, when it does not already have it.
///
/// Dropping privileges reintroduces the problem `updateRemoteUserUID` exists to solve: the
/// container has to start as the container user to run a Feature entrypoint, so the numeric
/// `--user` mapping cannot apply, and `su vscode` lands on whatever UID the image gave that
/// user — typically 1000. A host user who is not 1000 then cannot write their own worktree.
///
/// The reference CLI solves this by *rebuilding the image* with the UID changed. Doing it at
/// container start instead keeps one image per config rather than one per config-and-user, at
/// the cost of running on every start — which is cheap, because `usermod -u` rewrites a passwd
/// entry and re-owns a home directory that holds dotfiles.
///
/// Best-effort throughout: an image without `usermod` simply keeps the UID it had, which is the
/// behaviour this replaces. It must never take down a session that would otherwise have run.
fn align_uid_script(user: &str) -> Option<String> {
    let (uid, gid) = get_host_uid_gid()?;
    let user = shell_quote(user);
    Some(format!(
        "{{ [ \"$(id -u {user} 2>/dev/null)\" = {uid} ] \
         || {{ command -v usermod >/dev/null 2>&1 \
         && groupmod -g {gid} {user} >/dev/null 2>&1; \
         usermod -u {uid} -g {gid} {user} >/dev/null 2>&1; }}; }} || true"
    ))
}

/// The shell snippet that runs `userEnvProbe` and applies what it finds.
///
/// Mirrors the reference CLI: resolve the user's login shell, run it with the mode's flags, and
/// read `/proc/self/environ`, which is NUL-separated.
///
/// The loop that applies the result is line-based, because POSIX `sh` has no way to iterate
/// NUL-delimited records. So the single `tr` swaps the two delimiters at once: real newlines
/// inside values become `\001`, and the NULs between variables become newlines. One variable is
/// then exactly one line no matter what it contains, and the newlines are put back per value
/// before exporting. Without this a value like a multi-line `LS_COLORS` or a PEM key would be
/// truncated at its first newline — the remainder, having no `=`, would look like a malformed
/// entry and be dropped. (A value containing a literal `\001` would be corrupted instead; nothing
/// puts one in the environment, and losing an SOH beats losing a private key's body.)
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
         _am_env=$(\"$_am_shell\" {flags} 'cat /proc/self/environ' 2>/dev/null \
         | tr '\\n\\000' '\\001\\n')\n\
         _am_ifs=$IFS; IFS='\n'\n\
         for _am_line in $_am_env; do\n\
         {guard}\
         \x20   case \"$_am_line\" in *=*) ;; *) continue ;; esac\n\
         \x20   _am_val=$(printf '%s' \"${{_am_line#*=}}\" | tr '\\001' '\\n'; printf x)\n\
         \x20   export \"${{_am_line%%=*}}=${{_am_val%x}}\" 2>/dev/null || true\n\
         done\n\
         IFS=$_am_ifs; unset _am_shell _am_env _am_ifs _am_line _am_val"
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

/// Run a shell snippet inside an already-running container.
///
/// Used for `postAttachCommand`, the one lifecycle hook that fires against a container `am` did
/// not just create — so it cannot be chained into the command the way the others are.
pub fn exec_script(runtime: &ContainerRuntime, container_name: &str, script: &str) -> Result<()> {
    run_container_cmd(runtime, &["exec", container_name, "sh", "-c", script])
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

    /// A built-in's definition, resolved against an empty config — what every test that only
    /// cares about the compiled-in behaviour wants.
    fn profile(name: &str) -> crate::harness::Harness {
        crate::harness::resolve(name, &crate::config::Config::default())
            .expect("built-in agents resolve against a default config")
    }

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Every variable these tests set. Listing them here rather than restoring each at its own
    /// call site is what makes the restoration exhaustive by construction.
    const TOUCHED_ENV: [&str; 8] = [
        "AM_DOCKER_BIN",
        "AM_GH_BIN",
        "AM_PODMAN_BIN",
        "CLAUDE_CONFIG_DIR",
        "HOME",
        "MOCK_CONTAINER_LOG",
        "OPENAI_API_KEY",
        "SSH_AUTH_SOCK",
    ];

    /// Serialises the tests that mutate process-wide environment variables, and puts every one
    /// of them back when the test ends — including when it ends by panicking. Clearing a
    /// variable a test did not set is not the same as restoring it; see `EnvGuard`.
    fn lock_env() -> (std::sync::MutexGuard<'static, ()>, crate::test_support::EnvGuard) {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = crate::test_support::EnvGuard::saving(&TOUCHED_ENV);
        (lock, env)
    }

    #[test]
    fn agent_auto_flags_claude_returns_skip_permissions() {
        let flags = profile("claude").auto_flags;
        assert_eq!(flags, vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn agent_auto_flags_non_claude_agents_return_empty() {
        assert!(profile("codex").auto_flags.is_empty());
        assert!(profile("copilot").auto_flags.is_empty());
        assert!(profile("gemini").auto_flags.is_empty());
    }

    // ── resume flags (OQ-3) ────────────────────────────────────────────────────

    #[test]
    fn agent_resume_flags_claude_uses_continue() {
        assert_eq!(profile("claude").resume, Some(vec!["--continue".to_string()]));
    }

    #[test]
    fn agent_resume_flags_copilot_uses_continue() {
        assert_eq!(profile("copilot").resume, Some(vec!["--continue".to_string()]));
    }

    #[test]
    fn agent_resume_flags_gemini_uses_resume_latest() {
        assert_eq!(
            profile("gemini").resume,
            Some(vec!["--resume".to_string(), "latest".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_codex_uses_resume_subcommand() {
        assert_eq!(
            profile("codex").resume,
            Some(vec!["resume".to_string(), "--last".to_string()])
        );
    }

    #[test]
    fn agent_resume_flags_never_returns_none_by_accident() {
        // Every built-in was verified to support resuming (see the `resume` field's doc
        // comment on `harness::Harness`); pin that none of them silently regress to
        // "unsupported".
        for name in ["claude", "copilot", "gemini", "codex"] {
            assert!(profile(name).resume.is_some(), "{name} should support resume");
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
    fn docker_maps_the_host_uid_even_when_a_user_is_named() {
        // The old behaviour skipped the mapping whenever a user was named, which is only
        // harmless for a host user who happens to be uid 1000. For anyone else the container
        // could not write the worktree bind-mounted into it.
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            user: Some("vscode".to_string()),
            update_remote_user_uid: true,
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
        if let Some((uid, gid)) = get_host_uid_gid() {
            assert!(joined.contains(&format!("--user {uid}:{gid}")), "got: {joined}");
            // A bare uid has no passwd entry, so HOME has to be stated.
            assert!(joined.contains("-e HOME=/home/am"), "got: {joined}");
            assert!(!joined.contains("--user vscode"), "the two would contradict: {joined}");
        }
    }

    #[test]
    fn update_remote_user_uid_false_keeps_the_named_user() {
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            user: Some("vscode".to_string()),
            update_remote_user_uid: false,
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
        assert!(cmd.join(" ").contains("--user vscode"));
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

    /// The collision this exists for: a second session on the same repo forwards the same ports
    /// as the first, and `podman run` refuses to bind them — after everything else about the
    /// session has already been built.
    #[test]
    fn a_port_already_in_use_is_dropped_rather_than_failing_the_session() {
        use crate::devcontainer::ForwardedPort;
        // Stand in for the first session's published port. Bound on loopback, which is where
        // `forwardPorts` publishes.
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = held.local_addr().unwrap().port();
        let free = {
            let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            probe.local_addr().unwrap().port()
        };

        let mut ports = vec![
            ForwardedPort::Own(taken),
            ForwardedPort::Own(free),
            // No host port: the runtime picks one, so there is nothing to collide with.
            ForwardedPort::Published("9999".to_string()),
            ForwardedPort::Service { service: "db".into(), port: taken },
        ];
        let dropped = drop_busy_ports(&mut ports);

        assert_eq!(dropped, vec![taken]);
        assert_eq!(
            ports,
            vec![
                ForwardedPort::Own(free),
                ForwardedPort::Published("9999".to_string()),
                ForwardedPort::Service { service: "db".into(), port: taken },
            ],
            "only the colliding published port should have been dropped"
        );
    }

    #[test]
    fn the_host_half_of_a_publish_spec_is_read_the_way_the_runtime_reads_it() {
        assert_eq!(host_binding("127.0.0.1:3000:3000"), Some(("127.0.0.1".to_string(), 3000)));
        assert_eq!(host_binding("8080:80"), Some(("0.0.0.0".to_string(), 8080)));
        // The runtime assigns the host port, so there is nothing to preflight.
        assert_eq!(host_binding("80"), None);
        // A range is not a port; leave it to the runtime rather than guess.
        assert_eq!(host_binding("8000-8010:8000-8010"), None);
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

        let agent_auth = preflight_agent_auth(&profile("claude"), "/home/am").unwrap();
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
    fn build_run_command_drops_an_untrusted_devcontainer_bind_mount() {
        // Exercises the same trust policy (`devcontainer::apply_trust`) that
        // `compose::override_document` is exercised against below — the run-command and
        // Compose paths must agree on what a devcontainer is allowed to bind-mount.
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let worktree = mounts.worktree_host.clone();
        std::fs::create_dir_all(&worktree).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let resolved = crate::devcontainer::ResolvedConfig {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: Some(outside.to_string_lossy().into_owned()),
                target: "/host-secret".to_string(),
                kind: "bind".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        // Default config: allow_host_mounts is false.
        let dc = crate::devcontainer::apply_trust(
            &resolved,
            &crate::config::Config::default(),
            &worktree,
        );

        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &dc,
        );
        let joined = cmd.join(" ");
        assert!(
            !joined.contains("/host-secret") && !joined.contains(&outside.to_string_lossy().into_owned()),
            "untrusted bind mount must not reach the run command: {joined}"
        );
    }

    #[test]
    fn build_run_command_keeps_a_worktree_internal_devcontainer_bind_mount() {
        let tmp = TempDir::new().unwrap();
        let mounts = make_mounts(tmp.path());
        let worktree = mounts.worktree_host.clone();
        let sub = worktree.join("data");
        std::fs::create_dir_all(&sub).unwrap();

        let resolved = crate::devcontainer::ResolvedConfig {
            mounts: vec![crate::devcontainer::NormalizedMount {
                source: Some(sub.to_string_lossy().into_owned()),
                target: "/data".to_string(),
                kind: "bind".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };
        let dc = crate::devcontainer::apply_trust(
            &resolved,
            &crate::config::Config::default(),
            &worktree,
        );

        let cmd = build_run_command(
            &docker_runtime(),
            "ubuntu:25.10",
            &mounts,
            &[],
            &[],
            &NetworkMode::Full,
            "am-feat",
            &dc,
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains(&format!("{}:/data", sub.to_string_lossy())),
            "worktree-internal bind mount should reach the run command: {joined}"
        );
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
        assert!(!joined.contains("--security-opt"));
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
        compose_entrypoint_command(entrypoints, &[], agent_cmd, UserEnvProbe::None, &[], None)
    }

    #[test]
    fn no_entrypoints_leaves_the_agent_command_alone() {
        let agent = vec!["claude".to_string()];
        assert_eq!(compose_entrypoint_command(&[], &[], &agent, UserEnvProbe::None, &[], None), agent);
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
    fn entrypoints_run_as_the_container_user_and_the_agent_is_dropped_to_the_remote_one() {
        // Verified against the reference CLI: it starts the container as root, runs Feature
        // entrypoints there, and runs postCreateCommand and tools as remoteUser. A
        // docker-in-docker entrypoint starting dockerd, or sshd binding a privileged port,
        // cannot work any other way.
        let cmd = compose_entrypoint_command(
            &["/usr/local/share/docker-init.sh".to_string()],
            &["echo hook".to_string()],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            Some("vscode"),
        );
        let script = &cmd[2];
        // The entrypoint runs unwrapped, before the drop — it is the container user's.
        let entry_at = script.find("/usr/local/share/docker-init.sh").unwrap();
        let drop_at = script.find("exec su ").unwrap();
        assert!(entry_at < drop_at, "got: {script}");
        // Everything else is inside the drop.
        assert!(script.contains("exec su vscode -s /bin/sh -c "), "got: {script}");
        let dropped = script.split("-c ").nth(1).unwrap();
        assert!(dropped.contains("echo hook"), "hooks belong to the remote user: {script}");
        assert!(dropped.contains("exec claude"), "got: {script}");
    }

    #[test]
    fn dropping_privileges_realigns_the_users_uid_with_the_hosts() {
        // Dropping reintroduces exactly what `updateRemoteUserUID` exists to prevent: `su
        // vscode` lands on whatever UID the image gave that user, so a host user who is not
        // that UID cannot write their own worktree. The reference CLI rebuilds the image to
        // fix this; doing it at container start keeps one image per config instead of one per
        // config-and-user.
        let cmd = compose_entrypoint_command(
            &["/init.sh".to_string()],
            &[],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            Some("vscode"),
        );
        let script = &cmd[2];
        if let Some((uid, _)) = get_host_uid_gid() {
            assert!(script.contains(&format!("usermod -u {uid}")), "got: {script}");
            // Skipped when the UID already matches, so the common case costs nothing.
            assert!(script.contains(&format!(r#"[ "$(id -u vscode 2>/dev/null)" = {uid} ]"#)));
            // Best-effort: an image without usermod keeps the UID it had rather than failing.
            assert!(script.contains("|| true"), "got: {script}");
            let align_at = script.find("usermod").unwrap();
            let drop_at = script.find("exec su ").unwrap();
            assert!(align_at < drop_at, "alignment must precede the drop: {script}");
        }
    }

    #[test]
    fn nothing_is_realigned_without_a_drop() {
        let cmd = compose_entrypoint_command(
            &[],
            &[],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            None,
        );
        assert!(!cmd.join(" ").contains("usermod"));
    }

    #[test]
    fn without_an_entrypoint_nothing_is_dropped() {
        // Nothing needs elevation, so the container runs as the remote user directly and the
        // common path keeps exactly the shape it had.
        let cmd = compose_entrypoint_command(
            &[],
            &["echo hook".to_string()],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            None,
        );
        assert_eq!(cmd[2], "echo hook && exec claude");
    }

    #[test]
    fn a_failed_entrypoint_stops_the_session_before_the_drop() {
        let cmd = compose_entrypoint_command(
            &["/init.sh".to_string()],
            &[],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            Some("vscode"),
        );
        assert!(cmd[2].contains("/init.sh && exec su "), "got: {}", cmd[2]);
    }

    #[test]
    fn the_dropped_script_is_quoted_as_one_argument() {
        // The inner script carries quotes of its own — a hook with an argument, the probe's
        // `case` statement — and must survive being handed to `su -c`.
        let cmd = compose_entrypoint_command(
            &["/init.sh".to_string()],
            &["echo 'it works'".to_string()],
            &["claude".to_string()],
            UserEnvProbe::None,
            &[],
            Some("vscode"),
        );
        // Single-quoted with the inner quotes escaped the POSIX way: '\'' closes, escapes, reopens.
        let expected = concat!(r"'echo '", r"\", r"'", r"'it works'", r"\", r"'", r"' && exec claude'");
        assert!(cmd[2].contains(expected), "got: {}", cmd[2]);
    }

    #[test]
    fn dropping_privileges_and_the_uid_mapping_do_not_contradict_each_other() {
        // Both fixes touch `--user`. The container must start as the container user to run an
        // entrypoint, so the numeric mapping cannot also apply — asserting it here so a future
        // change to either cannot silently produce two conflicting `--user` flags.
        let tmp = TempDir::new().unwrap();
        let dc = DevcontainerRuntime {
            user: None,
            drop_to: Some("vscode".to_string()),
            update_remote_user_uid: true,
            entrypoints: vec!["/init.sh".to_string()],
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
        assert!(!joined.contains("--user"), "must start as the image's own user: {joined}");
    }

    #[test]
    fn a_probe_runs_before_the_agent_and_does_not_gate_it() {
        let cmd = compose_entrypoint_command(
            &[],
            &[],
            &["claude".to_string()],
            UserEnvProbe::LoginInteractiveShell,
            &[],
            None,
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
            &[],
            &["/hook.sh".to_string()],
            &["claude".to_string()],
            UserEnvProbe::LoginShell,
            &[],
            None,
        );
        assert!(cmd[2].contains("/hook.sh && exec claude"), "got: {}", cmd[2]);
    }

    #[test]
    fn each_probe_mode_maps_to_the_shell_flags_the_cli_uses() {
        let flags = |p: UserEnvProbe| {
            compose_entrypoint_command(&[], &[], &["a".to_string()], p, &[], None).join(" ")
        };
        assert!(flags(UserEnvProbe::LoginInteractiveShell).contains(" -lic "));
        assert!(flags(UserEnvProbe::LoginShell).contains(" -lc "));
        assert!(flags(UserEnvProbe::InteractiveShell).contains(" -ic "));
        // `none` must leave the command completely alone — no shell wrapper at all.
        assert_eq!(
            compose_entrypoint_command(&[], &[], &["a".to_string()], UserEnvProbe::None, &[], None),
            vec!["a".to_string()]
        );
    }

    /// The generated snippet is shell, so the only real check is running it.
    #[cfg(unix)]
    #[test]
    fn the_probe_script_applies_what_it_finds_and_respects_protected_names() {
        let cmd = compose_entrypoint_command(
            &[],
            &[],
            &["env".to_string()],
            UserEnvProbe::LoginShell,
            &["AM_KEEP".to_string()],
            None,
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

    /// A value with a newline in it is not exotic — `LS_COLORS` from some dotfile frameworks, a
    /// PEM-formatted key, a multi-line prompt. The probe reads a NUL-separated environ precisely
    /// so these survive; a line-based loop that splits on newline anyway would truncate the value
    /// at its first line and silently drop the rest.
    #[cfg(unix)]
    #[test]
    fn a_probed_value_containing_newlines_arrives_whole() {
        let cmd = compose_entrypoint_command(
            &[],
            &[],
            &["env".to_string()],
            UserEnvProbe::LoginShell,
            &[],
            None,
        );
        let tmp = TempDir::new().unwrap();
        let fake = tmp.path().join("fakeshell");
        std::fs::write(
            &fake,
            "#!/bin/sh\nAM_MULTI='first\nsecond\nthird' AM_AFTER=yes /usr/bin/env -0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let script = cmd[2].replace(
            "_am_shell=$(getent passwd \"$(id -u)\" 2>/dev/null | cut -d: -f7)",
            &format!("_am_shell={}", fake.display()),
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("running the generated script");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let env = String::from_utf8_lossy(&out.stdout);

        assert!(
            env.contains("AM_MULTI=first\nsecond\nthird"),
            "a multi-line value was truncated: {env}"
        );
        // The variable that followed it in environ must still have made it through: a split value
        // also derails the parse of everything after it.
        assert!(env.contains("AM_AFTER=yes"), "the variable after a multi-line one was lost: {env}");
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

        let mounts = resolve_credential_mounts(&profile("claude"), "/home/am").unwrap();
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

        let mounts = resolve_credential_mounts(&profile("claude"), "/home/am").unwrap();
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

    // Agent name validation (`AgentName::parse`/`parse_builtin`) now lives in `harness.rs`,
    // tested there — see `harness::tests`. `KnownAgent` no longer exists.

    // ── preflight_agent_auth ───────────────────────────────────────────

    #[test]
    fn preflight_agent_auth_claude_ok_when_dir_exists() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::fs::create_dir(tmp.path().join(".claude")).unwrap();

        assert!(preflight_agent_auth(&profile("claude"), "/home/am").is_ok());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_claude_fails_when_dir_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        assert!(preflight_agent_auth(&profile("claude"), "/home/am").is_err());

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

        let auth = preflight_agent_auth(&profile("copilot"), "/home/am").unwrap();
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

        assert!(preflight_agent_auth(&profile("copilot"), "/home/am").is_err());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_gemini_ok_when_dir_exists() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::fs::create_dir(tmp.path().join(".gemini")).unwrap();

        assert!(preflight_agent_auth(&profile("gemini"), "/home/am").is_ok());

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_gemini_fails_when_dir_missing() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());

        assert!(preflight_agent_auth(&profile("gemini"), "/home/am").is_err());

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

        let auth = preflight_agent_auth(&profile("codex"), "/home/am").unwrap();
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

        let err = preflight_agent_auth(&profile("codex"), "/home/am").unwrap_err();
        assert!(err.to_string().contains("OPENAI_API_KEY"));

        std::env::remove_var("HOME");
    }

    #[test]
    fn preflight_agent_auth_codex_fails_when_key_empty() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("OPENAI_API_KEY", "");

        let err = preflight_agent_auth(&profile("codex"), "/home/am").unwrap_err();
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
        let auth = preflight_agent_auth(&profile("codex"), "/home/am").unwrap();
        assert!(auth.env.is_empty());
        assert_eq!(auth.mounts.len(), 1);

        std::env::remove_var("HOME");
    }

    #[test]
    fn credentials_hint_names_a_concrete_command_per_agent() {
        assert!(credentials_hint(&profile("claude")).contains("claude auth login"));
        assert!(credentials_hint(&profile("claude")).contains("ANTHROPIC_API_KEY"));
        assert!(credentials_hint(&profile("claude"))
            .contains("https://dstanek.github.io/agent-manager/guides/claude-code/#prerequisites"));

        assert!(credentials_hint(&profile("copilot")).contains("gh auth login"));
        assert!(credentials_hint(&profile("copilot")).contains(
            "https://dstanek.github.io/agent-manager/guides/github-copilot/#prerequisites"
        ));

        assert!(credentials_hint(&profile("gemini")).contains("Gemini CLI"));
        assert!(credentials_hint(&profile("gemini"))
            .contains("https://dstanek.github.io/agent-manager/guides/gemini/#prerequisites"));

        assert!(credentials_hint(&profile("codex")).contains("codex"));
        assert!(credentials_hint(&profile("codex")).contains("OPENAI_API_KEY"));
        assert!(credentials_hint(&profile("codex"))
            .contains("https://dstanek.github.io/agent-manager/guides/codex/#prerequisites"));
    }

    #[test]
    fn codex_mounts_the_config_dir_read_write_when_present() {
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::env::set_var("HOME", tmp.path());

        let mounts = resolve_credential_mounts(&profile("codex"), "/home/am").unwrap();
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

        let auth_mounts = resolve_credential_mounts(&profile("copilot"), "/home/am").unwrap();
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

        let auth_mounts = resolve_credential_mounts(&profile("copilot"), "/home/am").unwrap();
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

        let auth_mounts = resolve_credential_mounts(&profile("gemini"), "/home/am").unwrap();
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
        assert!(resolve_credential_mounts(&profile("codex"), "/home/am")
            .unwrap()
            .is_empty());

        std::env::remove_var("HOME");
    }
}
