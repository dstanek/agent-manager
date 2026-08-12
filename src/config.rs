use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Enum types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    #[default]
    Git,
    Jj,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PaneSide {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    #[default]
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuntimePreference {
    #[default]
    Auto,
    Podman,
    Docker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    Full,
    None,
}

/// Where a session's container environment comes from.
///
/// `Auto` is the default: a repo that has taken the trouble to describe its environment in
/// `.devcontainer/devcontainer.json` almost certainly means for that description to be
/// used, and preferring an `am`-specific image over it is the surprising behaviour. Repos
/// without a config are unaffected, since `Auto` falls back to an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContainerMode {
    /// An `am`-resolved image (`container.image` or `agents.<name>.image`).
    Image,
    /// The repo's own `.devcontainer/devcontainer.json`; error if there isn't one.
    Devcontainer,
    /// Devcontainer when a config is discovered, image otherwise.
    #[default]
    Auto,
}

/// How the agent gets into a devcontainer image, which the project's own config
/// naturally knows nothing about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentInstall {
    /// Inject a devcontainer Feature at build time, baked into the cached image.
    Feature,
    /// Install into a named volume shared across sessions. Works on any base image.
    Bootstrap,
    /// The devcontainer already provides the agent.
    None,
    /// `feature` if one is mapped for the agent, else `bootstrap`.
    #[default]
    Auto,
}

// ── Config structs ────────────────────────────────────────────────────────────

/// Per-agent configuration (image override, devcontainer Feature, etc.)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentSettings {
    pub image: Option<String>,
    /// Devcontainer Feature that installs this agent, injected via
    /// `--additional-features` when `devcontainer.agent_install` selects it.
    pub devcontainer_feature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxConfig {
    pub agent_pane: PaneSide,
    pub split: SplitDirection,
    #[serde(
        deserialize_with = "deserialize_split_percent",
        serialize_with = "serialize_split_percent"
    )]
    pub split_percent: u8,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            agent_pane: PaneSide::Left,
            split: SplitDirection::Horizontal,
            split_percent: 50,
        }
    }
}

fn deserialize_split_percent<'de, D>(deserializer: D) -> std::result::Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = u8::deserialize(deserializer)?;
    if !(1..=99).contains(&val) {
        return Err(serde::de::Error::custom(
            "split_percent must be between 1 and 99 (percentage of window for agent pane)",
        ));
    }
    Ok(val)
}

fn serialize_split_percent<S>(value: &u8, serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_u8(*value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    pub enabled: bool,
    pub mode: ContainerMode,
    pub runtime: RuntimePreference,
    pub image: Option<String>,
    pub network: NetworkMode,
    pub env: Vec<String>,
    pub gitconfig: Option<PathBuf>, // None = ~/.gitconfig
    pub ssh: Option<PathBuf>,       // None = ~/.ssh
    /// Forward the host's `SSH_AUTH_SOCK` into the container.
    ///
    /// On by default. Mounting `~/.ssh` is not enough on its own: a passphrase-protected
    /// key cannot be decrypted without a prompt, and keys held only in an agent
    /// (1Password, gnome-keyring, a FIDO token) never appear in `~/.ssh` at all. Without
    /// this the failure is silent until a `git push` inside the session fails.
    pub ssh_agent: bool,
    pub user: String, // container username (default: "am")
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ContainerMode::default(),
            runtime: RuntimePreference::Auto,
            image: None,
            network: NetworkMode::Full,
            env: Vec::new(),
            gitconfig: None,
            ssh: None,
            ssh_agent: true,
            user: "am".to_string(),
        }
    }
}

/// Settings that only apply when a session's environment comes from a
/// `devcontainer.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevcontainerConfig {
    /// Explicit config path, relative to the session worktree. `None` = discover.
    pub path: Option<PathBuf>,
    /// The `devcontainer` CLI binary. Overridden by `AM_DEVCONTAINER_BIN`.
    pub cli: String,
    pub agent_install: AgentInstall,
    /// Whether `initializeCommand` may run on the host. Off by default —
    /// `am` exists to isolate agents, and this is repo-controlled code.
    pub allow_host_commands: bool,
    /// Skip every in-container lifecycle hook.
    pub skip_lifecycle: bool,
    /// Override the container home derived from `remoteUser`/`containerUser`.
    pub home: Option<PathBuf>,
    /// Extra Features to inject at build time, as `id -> options JSON`.
    pub extra_features: HashMap<String, String>,
}

impl Default for DevcontainerConfig {
    fn default() -> Self {
        Self {
            path: None,
            cli: "devcontainer".to_string(),
            agent_install: AgentInstall::default(),
            allow_host_commands: false,
            skip_lifecycle: false,
            home: None,
            extra_features: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub agent: Option<String>,
    /// Per-agent settings (image, etc.). Compiled-in defaults for known agents.
    pub agents: HashMap<String, AgentSettings>,
    pub tmux: TmuxConfig,
    pub container: ContainerConfig,
    pub devcontainer: DevcontainerConfig,
    /// Keys found in the config files that `am` does not recognise. Not a config value
    /// itself, so it never round-trips through a config file.
    #[serde(skip)]
    pub unknown_keys: Vec<UnknownKey>,
}

/// Compiled-in per-agent defaults.
///
/// Only Claude Code has an official devcontainer Feature today; the others have no
/// published Feature, so they fall through to the bootstrap install path.
fn default_agent_images() -> HashMap<String, AgentSettings> {
    [
        (
            "claude",
            Some("ghcr.io/dstanek/am-claude-minimal:latest"),
            Some("ghcr.io/anthropics/devcontainer-features/claude-code:1"),
        ),
        (
            "copilot",
            Some("ghcr.io/dstanek/am-copilot-minimal:latest"),
            None,
        ),
    ]
    .into_iter()
    .map(|(name, image, feature)| {
        (
            name.to_string(),
            AgentSettings {
                image: image.map(str::to_string),
                devcontainer_feature: feature.map(str::to_string),
            },
        )
    })
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: None,
            agents: default_agent_images(),
            tmux: TmuxConfig::default(),
            container: ContainerConfig::default(),
            devcontainer: DevcontainerConfig::default(),
            unknown_keys: Vec::new(),
        }
    }
}

/// Resolve the devcontainer Feature that installs a given agent, if one is mapped.
pub fn resolve_agent_feature<'a>(agent: Option<&str>, cfg: &'a Config) -> Option<&'a str> {
    cfg.agents
        .get(agent?)?
        .devcontainer_feature
        .as_deref()
        .filter(|s| !s.is_empty())
}

/// Resolve the container image for a given agent name.
///
/// Resolution order (first match wins):
/// 1. `container.image` — explicit override for custom images
/// 2. `agents[name].image` — agent-specific image (compiled-in defaults or user config)
pub fn resolve_image<'a>(agent: Option<&str>, cfg: &'a Config) -> Option<&'a str> {
    if let Some(img) = cfg.container.image.as_deref().filter(|s| !s.is_empty()) {
        return Some(img);
    }
    if let Some(name) = agent {
        if let Some(settings) = cfg.agents.get(name) {
            return settings.image.as_deref().filter(|s| !s.is_empty());
        }
    }
    None
}

// ── TOML file shapes (partial overrides allowed) ──────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct FileDefaults {
    agent: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileAgentSettings {
    image: Option<String>,
    devcontainer_feature: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileTmux {
    agent_pane: Option<PaneSide>,
    split: Option<SplitDirection>,
    split_percent: Option<u8>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileContainer {
    enabled: Option<bool>,
    mode: Option<ContainerMode>,
    runtime: Option<RuntimePreference>,
    image: Option<String>,
    network: Option<NetworkMode>,
    env: Option<Vec<String>>,
    gitconfig: Option<PathBuf>,
    ssh: Option<PathBuf>,
    ssh_agent: Option<bool>,
    user: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileDevcontainer {
    path: Option<PathBuf>,
    cli: Option<String>,
    agent_install: Option<AgentInstall>,
    allow_host_commands: Option<bool>,
    skip_lifecycle: Option<bool>,
    home: Option<PathBuf>,
    extra_features: Option<HashMap<String, String>>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    defaults: FileDefaults,
    #[serde(default)]
    agents: HashMap<String, FileAgentSettings>,
    #[serde(default)]
    tmux: FileTmux,
    #[serde(default)]
    container: FileContainer,
    #[serde(default)]
    devcontainer: FileDevcontainer,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

/// A key `am` does not recognise, and the file it came from.
///
/// Collected rather than rejected. `deny_unknown_fields` would turn a typo into a hard
/// error, but it would also break a config written for a newer `am` when an older one
/// reads it — and `.am/config.toml` is meant to be committed and shared, so that is a
/// team hitting version skew, not a hypothetical. Warning keeps the diagnosis without
/// the breakage.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownKey {
    pub file: PathBuf,
    pub key: String,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in {}", self.key, self.file.display())
    }
}

/// Every unrecognised key in a parsed file, as dotted paths.
///
/// Sorted so the output is stable — `HashMap` iteration order would otherwise reshuffle
/// the warning on every run and make it look like the set had changed.
fn collect_unknown(file: &FileConfig, path: &Path) -> Vec<UnknownKey> {
    let mut keys: Vec<String> = Vec::new();
    keys.extend(file.unknown.keys().cloned());
    keys.extend(file.defaults.unknown.keys().map(|k| format!("defaults.{k}")));
    keys.extend(file.tmux.unknown.keys().map(|k| format!("tmux.{k}")));
    keys.extend(file.container.unknown.keys().map(|k| format!("container.{k}")));
    keys.extend(
        file.devcontainer
            .unknown
            .keys()
            .map(|k| format!("devcontainer.{k}")),
    );
    for (name, settings) in &file.agents {
        keys.extend(
            settings
                .unknown
                .keys()
                .map(|k| format!("agents.{name}.{k}")),
        );
    }
    keys.sort();
    keys.into_iter()
        .map(|key| UnknownKey {
            file: path.to_path_buf(),
            key,
        })
        .collect()
}

/// Overwrite `target` with `value` when present. `target` is a plain `T` (not `Option<T>`).
fn apply_opt<T: Clone>(target: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *target = v;
    }
}

/// Overwrite `target` with `Some(value)` when present. `target` is an `Option<T>`;
/// any non-None value (including empty paths) is accepted.
fn apply_opt_some<T: Clone>(target: &mut Option<T>, value: Option<T>) {
    if let Some(v) = value {
        *target = Some(v);
    }
}

/// Overwrite `target` with `Some(value)` when present and non-empty.
/// Empty strings are ignored so that a blank config entry does not clear an existing value.
fn apply_opt_string(target: &mut Option<String>, value: Option<String>) {
    if let Some(v) = value {
        if !v.is_empty() {
            *target = Some(v);
        }
    }
}

fn apply_file_config(base: &mut Config, file: FileConfig) {
    apply_opt_string(&mut base.agent, file.defaults.agent);

    // Merge agents: file entries extend/override the compiled-in defaults.
    for (name, file_agent) in file.agents {
        let entry = base.agents.entry(name).or_default();
        apply_opt_string(&mut entry.image, file_agent.image);
        apply_opt_string(
            &mut entry.devcontainer_feature,
            file_agent.devcontainer_feature,
        );
    }

    apply_opt(&mut base.tmux.agent_pane, file.tmux.agent_pane);
    apply_opt(&mut base.tmux.split, file.tmux.split);
    apply_opt(&mut base.tmux.split_percent, file.tmux.split_percent);

    apply_opt(&mut base.container.enabled, file.container.enabled);
    apply_opt(&mut base.container.mode, file.container.mode);
    apply_opt(&mut base.container.runtime, file.container.runtime);
    apply_opt_string(&mut base.container.image, file.container.image);
    apply_opt(&mut base.container.network, file.container.network);
    apply_opt(&mut base.container.env, file.container.env);
    apply_opt_some(&mut base.container.gitconfig, file.container.gitconfig);
    apply_opt_some(&mut base.container.ssh, file.container.ssh);
    apply_opt(&mut base.container.ssh_agent, file.container.ssh_agent);
    if let Some(u) = file.container.user {
        if !u.is_empty() {
            base.container.user = u;
        }
    }

    apply_opt_some(&mut base.devcontainer.path, file.devcontainer.path);
    if let Some(cli) = file.devcontainer.cli {
        if !cli.is_empty() {
            base.devcontainer.cli = cli;
        }
    }
    apply_opt(
        &mut base.devcontainer.agent_install,
        file.devcontainer.agent_install,
    );
    apply_opt(
        &mut base.devcontainer.allow_host_commands,
        file.devcontainer.allow_host_commands,
    );
    apply_opt(
        &mut base.devcontainer.skip_lifecycle,
        file.devcontainer.skip_lifecycle,
    );
    apply_opt_some(&mut base.devcontainer.home, file.devcontainer.home);
    // Features merge key-by-key so a project can add one without restating the global set.
    if let Some(features) = file.devcontainer.extra_features {
        base.devcontainer.extra_features.extend(features);
    }
}

fn parse_config_file(path: &Path) -> Result<FileConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let file: FileConfig =
        toml::from_str(&text).with_context(|| format!("parsing config file {}", path.display()))?;
    Ok(file)
}

/// Returns the global config path: `$XDG_CONFIG_HOME/am/config.toml` if set,
/// otherwise `~/.config/am/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    dirs_path().map(|d| d.join("config.toml"))
}

fn dirs_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("am"))
}

/// Returns the global state directory: `$XDG_STATE_HOME/am` if set,
/// otherwise `~/.local/state/am`.
/// Returns `None` only if neither `XDG_STATE_HOME` nor `HOME` is set.
pub fn global_state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("am"))
}

/// The project config skeleton — the text `am init` writes (via `write_defaults` below), and
/// the base that `am setup`'s agent-aware variant (`onboarding::render_project_config_
/// skeleton_with_agent`) starts from, so the two front doors cannot produce different repos.
///
/// Fully commented out: a project file that activated anything would silently override the
/// user's global config the moment it was created.
pub fn render_project_config_skeleton() -> &'static str {
    r#"# Project-level am configuration — .am/config.toml
# Uncomment only the values you want to override from your global or compiled-in defaults.
# Precedence (highest wins): CLI flags > environment variables > project config > global config
# Run `am generate-config` to see the full global config template with all options documented.

[defaults]
# agent = "claude"       # agent to launch, e.g. "claude" | "copilot" — also selects the container image

# Override the container image for a specific agent (built-in defaults shown):
# [agents.claude]
# image = "ghcr.io/dstanek/am-claude-minimal:latest"
#
# [agents.copilot]
# image = "ghcr.io/dstanek/am-copilot-minimal:latest"

[tmux]
# agent_pane = "left"    # which pane gets the agent: "left" | "right"
# split = "horizontal"   # split direction: "horizontal" | "vertical"
# split_percent = 50     # percentage of the window given to the agent pane

[container]
# enabled = true
# mode = "auto"          # "auto" (devcontainer when one is found) | "devcontainer" | "image"
# runtime = "auto"       # "auto" | "podman" | "docker"
# network = "full"       # "full" | "none"
# env = []               # extra environment variables to pass into the container
# gitconfig = ""         # path to gitconfig to mount (default: ~/.gitconfig)
# ssh = ""               # path to SSH dir to mount (default: ~/.ssh)
# ssh_agent = true       # forward the host's SSH_AUTH_SOCK into the container
# image = ""             # override image for all agents (advanced; prefer [agents.<name>].image)
# user = "am"            # username inside the container (used for credential mount paths)

# Applies only when container.mode resolves to "devcontainer".
[devcontainer]
# path = ""                  # explicit devcontainer.json, relative to the worktree
# agent_install = "auto"     # "feature" | "bootstrap" | "none" | "auto"
# allow_host_commands = false # let initializeCommand run on the HOST — off by default
# skip_lifecycle = false     # skip postCreateCommand and friends
"#
}

/// Write the default project config file at `path` (creates parent directories as needed).
/// The file is written as a fully-commented-out template so it never silently overrides
/// global or compiled-in defaults.
pub fn write_defaults(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_project_config_skeleton())?;
    Ok(())
}

/// Returns the full global config template as a static string with all options active
/// and documented with inline comments.
pub fn global_config_template() -> &'static str {
    r#"# am global configuration — ~/.config/am/config.toml
# Sets machine-wide defaults for all projects.
# Precedence (highest wins): CLI flags > environment variables > project config (.am/config.toml) > global config
#
# Environment variable overrides:
#   AM_AGENT
#   AM_TMUX_AGENT_PANE, AM_TMUX_SPLIT, AM_TMUX_SPLIT_PERCENT
#   AM_CONTAINER_ENABLED, AM_CONTAINER_MODE, AM_CONTAINER_RUNTIME, AM_CONTAINER_IMAGE,
#   AM_CONTAINER_NETWORK, AM_CONTAINER_SSH_AGENT, AM_CONTAINER_USER
#   AM_DEVCONTAINER_PATH, AM_DEVCONTAINER_AGENT_INSTALL, AM_DEVCONTAINER_ALLOW_HOST_COMMANDS
#   AM_DEVCONTAINER_BIN (path to the `devcontainer` CLI)

[defaults]
agent = "claude"       # agent to launch; also selects the container image via [agents.<name>]

# Per-agent configuration. These are the compiled-in defaults — override here if needed.
# `image` is used in container.mode = "image"; `devcontainer_feature` is injected at build
# time in devcontainer mode when agent_install resolves to "feature".
[agents.claude]
image = "ghcr.io/dstanek/am-claude-minimal:latest"
devcontainer_feature = "ghcr.io/anthropics/devcontainer-features/claude-code:1"

[agents.copilot]
image = "ghcr.io/dstanek/am-copilot-minimal:latest"

# Add entries for any other agent you use, e.g.:
# [agents.gemini]
# image = "ghcr.io/your-org/am-gemini:latest"

[tmux]
agent_pane = "left"    # which pane gets the agent: "left" | "right"
split = "horizontal"   # split direction: "horizontal" | "vertical"
split_percent = 50     # percentage of the window given to the agent pane (1-99)

[container]
enabled = true
mode = "auto"          # where the environment comes from:
                       #   "auto"         — the repo's .devcontainer/devcontainer.json when
                       #                    one is found, an am-resolved image otherwise
                       #   "devcontainer" — the repo's config; error if there isn't one
                       #   "image"        — an am-resolved image, ignoring any .devcontainer/
runtime = "auto"       # "auto" (podman first, then docker) | "podman" | "docker"
network = "full"       # "full" (unrestricted) | "none" (no network access)
env = []               # extra environment variables passed into the container, e.g. ["FOO=bar"]
# gitconfig = ""        # path to gitconfig to mount (default: ~/.gitconfig)
# ssh = ""              # path to SSH dir to mount (default: ~/.ssh)
ssh_agent = true       # forward the host's SSH_AUTH_SOCK, so a passphrase-protected or
                       #   agent-only key still works for git push inside the session
# image = ""            # override image for all agents (advanced; prefer [agents.<name>].image above)
# user = "am"           # username inside the container (used for credential mount paths)

# Applies only when container.mode resolves to "devcontainer". Building requires the
# `devcontainer` CLI (npm install -g @devcontainers/cli) and Node 20+. am builds the image
# once per config change and runs it itself, so the CLI is not on the hot path.
[devcontainer]
# path = ""                    # explicit devcontainer.json, relative to the session worktree;
                               # default is to discover .devcontainer/devcontainer.json
cli = "devcontainer"           # CLI binary name or path (AM_DEVCONTAINER_BIN overrides)
agent_install = "auto"         # how the agent gets into the image:
                               #   "feature"   — inject the agent's devcontainer Feature at build
                               #   "bootstrap" — install into a shared volume at run time
                               #   "none"      — the devcontainer already provides it
                               #   "auto"      — feature if one is mapped, else bootstrap
allow_host_commands = false    # let initializeCommand run on YOUR HOST, outside the container.
                               # Off by default: devcontainer.json is repo-controlled code.
skip_lifecycle = false         # skip postCreateCommand and the other in-container hooks
# home = ""                    # override the container home derived from remoteUser

# Extra Features to inject at build time, as id -> options JSON:
# [devcontainer.extra_features]
# "ghcr.io/devcontainers/features/node:1" = "{}"
"#
}

/// Read environment variables and apply them to the config.
///
/// Unknown/unrecognised values for enum fields are silently ignored so that
/// adding new enum variants is backwards-compatible. Numeric fields with hard
/// physical constraints (e.g. split_percent must be 1–99) return an error
/// rather than silently falling back, matching the behaviour of TOML parsing.
fn apply_env_vars(config: &mut Config) -> Result<()> {
    if let Ok(val) = std::env::var("AM_AGENT") {
        if !val.is_empty() {
            config.agent = Some(val);
        }
    }
    if let Ok(val) = std::env::var("AM_TMUX_AGENT_PANE") {
        match val.as_str() {
            "left" => config.tmux.agent_pane = PaneSide::Left,
            "right" => config.tmux.agent_pane = PaneSide::Right,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_TMUX_SPLIT") {
        match val.as_str() {
            "horizontal" => config.tmux.split = SplitDirection::Horizontal,
            "vertical" => config.tmux.split = SplitDirection::Vertical,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_TMUX_SPLIT_PERCENT") {
        match val.parse::<u8>() {
            Ok(n) if (1..=99).contains(&n) => config.tmux.split_percent = n,
            _ => {
                return Err(anyhow::anyhow!(
                    "invalid AM_TMUX_SPLIT_PERCENT '{val}': must be a number between 1 and 99"
                ))
            }
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_ENABLED") {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => config.container.enabled = true,
            "false" | "0" | "no" => config.container.enabled = false,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_MODE") {
        match val.as_str() {
            "image" => config.container.mode = ContainerMode::Image,
            "devcontainer" => config.container.mode = ContainerMode::Devcontainer,
            "auto" => config.container.mode = ContainerMode::Auto,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_RUNTIME") {
        match val.as_str() {
            "auto" => config.container.runtime = RuntimePreference::Auto,
            "podman" => config.container.runtime = RuntimePreference::Podman,
            "docker" => config.container.runtime = RuntimePreference::Docker,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_IMAGE") {
        if !val.is_empty() {
            config.container.image = Some(val);
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_NETWORK") {
        match val.as_str() {
            "full" => config.container.network = NetworkMode::Full,
            "none" => config.container.network = NetworkMode::None,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_GITCONFIG") {
        if !val.is_empty() {
            config.container.gitconfig = Some(PathBuf::from(val));
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_SSH") {
        if !val.is_empty() {
            config.container.ssh = Some(PathBuf::from(val));
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_SSH_AGENT") {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => config.container.ssh_agent = true,
            "false" | "0" | "no" => config.container.ssh_agent = false,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_CONTAINER_USER") {
        if !val.is_empty() {
            config.container.user = val;
        }
    }
    if let Ok(val) = std::env::var("AM_DEVCONTAINER_PATH") {
        if !val.is_empty() {
            config.devcontainer.path = Some(PathBuf::from(val));
        }
    }
    if let Ok(val) = std::env::var("AM_DEVCONTAINER_AGENT_INSTALL") {
        match val.as_str() {
            "feature" => config.devcontainer.agent_install = AgentInstall::Feature,
            "bootstrap" => config.devcontainer.agent_install = AgentInstall::Bootstrap,
            "none" => config.devcontainer.agent_install = AgentInstall::None,
            "auto" => config.devcontainer.agent_install = AgentInstall::Auto,
            _ => {}
        }
    }
    if let Ok(val) = std::env::var("AM_DEVCONTAINER_ALLOW_HOST_COMMANDS") {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => config.devcontainer.allow_host_commands = true,
            "false" | "0" | "no" => config.devcontainer.allow_host_commands = false,
            _ => {}
        }
    }
    Ok(())
}

/// Validate that every entry in `container.env` is a valid env var pass-through.
/// Each entry must be either `NAME` or `NAME=value` where NAME is a non-empty
/// identifier (letters, digits, underscores). Entries starting with `-` would
/// be passed as flags to the container runtime, producing confusing errors.
fn validate_env_passthrough(env: &[String]) -> Result<()> {
    for entry in env {
        let name = entry.split('=').next().unwrap_or("");
        let valid = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(anyhow::anyhow!(
                "invalid container.env entry '{entry}': must be 'NAME' or 'NAME=value' \
                 where NAME starts with a letter or underscore and contains only \
                 letters, digits, and underscores"
            ));
        }
    }
    Ok(())
}

fn validate_split_percent(percent: u8) -> Result<()> {
    if !(1..=99).contains(&percent) {
        return Err(anyhow::anyhow!(
            "invalid tmux.split_percent {percent}: must be between 1 and 99"
        ));
    }
    Ok(())
}

fn validate_container_user(user: &str) -> Result<()> {
    let valid = !user.is_empty()
        && user
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && user
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        return Err(anyhow::anyhow!(
            "invalid container.user '{user}': must start with a lowercase letter or underscore and contain only lowercase letters, digits, underscores, and hyphens"
        ));
    }
    Ok(())
}

pub fn load_with_global(
    global_path: Option<&Path>,
    project_config_path: Option<&Path>,
) -> Result<Config> {
    let mut config = Config::default();

    // Apply global config if it exists
    if let Some(global_path) = global_path {
        if global_path.exists() {
            let file = parse_config_file(global_path)?;
            config.unknown_keys.extend(collect_unknown(&file, global_path));
            apply_file_config(&mut config, file);
        }
    }

    // Apply project config if provided and exists
    if let Some(path) = project_config_path {
        if path.exists() {
            let file = parse_config_file(path)?;
            config.unknown_keys.extend(collect_unknown(&file, path));
            apply_file_config(&mut config, file);
        }
    }

    // Apply environment variable overrides (highest precedence after CLI flags)
    apply_env_vars(&mut config)?;

    validate_split_percent(config.tmux.split_percent)?;
    validate_env_passthrough(&config.container.env)?;
    validate_container_user(&config.container.user)?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_toml(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    // ── Unknown keys ──────────────────────────────────────────────────────────

    #[test]
    fn unknown_keys_are_collected_not_rejected() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let project = write_toml(
            tmp.path(),
            "project.toml",
            r#"
typo_at_top = 1

[defaults]
agent = "claude"
agnet = "copilot"

[tmux]
splt_percent = 30

[container]
agent = "copilot"

[devcontainer]
skip_lifecyle = true

[agents.claude]
imag = "nope:latest"
"#,
        );

        let config = load_with_global(None, Some(&project)).unwrap();

        // The recognised key still applies — an unknown neighbour does not poison it.
        assert_eq!(config.agent.as_deref(), Some("claude"));

        let keys: Vec<&str> = config
            .unknown_keys
            .iter()
            .map(|u| u.key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec![
                "agents.claude.imag",
                "container.agent",
                "defaults.agnet",
                "devcontainer.skip_lifecyle",
                "tmux.splt_percent",
                "typo_at_top",
            ]
        );
        assert!(config.unknown_keys.iter().all(|u| u.file == project));
    }

    #[test]
    fn a_clean_config_reports_no_unknown_keys() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let project = write_toml(
            tmp.path(),
            "project.toml",
            "[defaults]\nagent = \"claude\"\n\n[container]\nenabled = true\n",
        );

        let config = load_with_global(None, Some(&project)).unwrap();
        assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);
    }

    #[test]
    fn unknown_keys_name_the_file_they_came_from() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let global = write_toml(tmp.path(), "global.toml", "[tmux]\nwrong_one = 1\n");
        let project = write_toml(tmp.path(), "project.toml", "[tmux]\nother_one = 2\n");

        let config = load_with_global(Some(&global), Some(&project)).unwrap();

        // Two files, same section: without the path a user cannot tell which to edit.
        let found: Vec<(String, String)> = config
            .unknown_keys
            .iter()
            .map(|u| (u.key.clone(), u.file.display().to_string()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("tmux.wrong_one".to_string(), global.display().to_string()),
                ("tmux.other_one".to_string(), project.display().to_string()),
            ]
        );
    }

    // ── Agent precedence ──────────────────────────────────────────────────────
    //
    // The generated config states one rule: CLI flags > env vars > project > global.
    // These pin the two file-and-env halves of it. `container.agent` used to break
    // both, because it resolved per key: a global container.agent beat a project
    // defaults.agent, and a project container.agent beat AM_AGENT. Any future
    // second way to name the agent must not reintroduce that.

    #[test]
    fn project_agent_beats_global_agent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let global = write_toml(tmp.path(), "global.toml", "[defaults]\nagent = \"copilot\"\n");
        let project = write_toml(tmp.path(), "project.toml", "[defaults]\nagent = \"claude\"\n");

        let config = load_with_global(Some(&global), Some(&project)).unwrap();
        assert_eq!(config.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn env_agent_beats_both_config_files() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::set_var("AM_AGENT", "gemini");
        let tmp = TempDir::new().unwrap();

        let global = write_toml(tmp.path(), "global.toml", "[defaults]\nagent = \"copilot\"\n");
        let project = write_toml(tmp.path(), "project.toml", "[defaults]\nagent = \"claude\"\n");

        let config = load_with_global(Some(&global), Some(&project)).unwrap();
        assert_eq!(config.agent.as_deref(), Some("gemini"));
    }

    #[test]
    fn container_agent_is_no_longer_a_way_to_set_the_agent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT", "AM_CONTAINER_AGENT"]);
        std::env::remove_var("AM_AGENT");
        // Neither the retired file key nor its env var may resurrect the inversion.
        std::env::set_var("AM_CONTAINER_AGENT", "copilot");
        let tmp = TempDir::new().unwrap();

        let project = write_toml(
            tmp.path(),
            "project.toml",
            "[container]\nagent = \"copilot\"\n",
        );

        // Unknown keys are ignored rather than rejected — no section in this file
        // sets `deny_unknown_fields` — so a stale config still loads. What matters
        // is that it no longer selects an agent behind `defaults.agent`'s back.
        let config = load_with_global(None, Some(&project)).unwrap();
        assert_eq!(config.agent, None);
    }

    #[test]
    fn defaults_when_no_config_files() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();
        let nonexistent_global = tmp.path().join("global.toml");
        let nonexistent_project = tmp.path().join("project.toml");

        let config =
            load_with_global(Some(&nonexistent_global), Some(&nonexistent_project)).unwrap();

        assert!(config.agent.is_none());
        assert_eq!(config.tmux.split_percent, 50);
        assert!(config.container.enabled);
        assert_eq!(config.container.runtime, RuntimePreference::Auto);
        assert!(config.container.image.is_none());
        // Compiled-in defaults provide images for known agents
        assert_eq!(
            config.agents.get("claude").and_then(|a| a.image.as_deref()),
            Some("ghcr.io/dstanek/am-claude-minimal:latest")
        );
        assert_eq!(
            config
                .agents
                .get("copilot")
                .and_then(|a| a.image.as_deref()),
            Some("ghcr.io/dstanek/am-copilot-minimal:latest")
        );
    }

    #[test]
    fn project_config_overrides_global() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let global_path = write_toml(
            tmp.path(),
            "global.toml",
            r#"
[defaults]
agent = "codex"
[container]
image = "global-image"
"#,
        );

        let project_path = write_toml(
            tmp.path(),
            "project.toml",
            r#"
[defaults]
agent = "claude"
[container]
image = "project-image"
"#,
        );

        let config = load_with_global(Some(&global_path), Some(&project_path)).unwrap();

        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.container.image.as_deref(), Some("project-image"));
    }

    #[test]
    fn project_config_inherits_unset_global_fields() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let global_path = write_toml(
            tmp.path(),
            "global.toml",
            r#"
[defaults]
agent = "claude"
[tmux]
split_percent = 70
"#,
        );

        // Project config only sets image, doesn't touch agent or split_percent
        let project_path = write_toml(
            tmp.path(),
            "project.toml",
            r#"
[container]
image = "myimage"
"#,
        );

        let config = load_with_global(Some(&global_path), Some(&project_path)).unwrap();

        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.tmux.split_percent, 70);
        assert_eq!(config.container.image.as_deref(), Some("myimage"));
    }

    #[test]
    fn write_defaults_creates_file_and_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("config.toml");
        write_defaults(&path).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[defaults]"));
        assert!(content.contains("[tmux]"));
        assert!(content.contains("[container]"));
    }

    #[test]
    fn write_defaults_content_is_valid_toml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        write_defaults(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: Result<toml::Value, _> = toml::from_str(&content);
        assert!(parsed.is_ok(), "default config is not valid TOML");
    }

    // Mutex to serialise all tests that mutate process-global env vars.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that saves the current value of env vars on construction and
    /// restores them on drop. Prevents test pollution when a test panics or
    /// fails an assertion before its manual `remove_var` calls.
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        /// Save the current values of `keys` so they are restored on drop.
        /// The caller is still responsible for calling `set_var`/`remove_var`
        /// to establish the desired state; this guard only handles cleanup.
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

    #[test]
    fn env_vars_override_project_config() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT", "AM_CONTAINER_IMAGE"]);
        let tmp = TempDir::new().unwrap();

        let project_path = write_toml(
            tmp.path(),
            "project.toml",
            r#"
[defaults]
agent = "claude"
[container]
image = "project-image"
"#,
        );

        std::env::set_var("AM_AGENT", "codex");
        std::env::set_var("AM_CONTAINER_IMAGE", "env-image");

        let config = load_with_global(None, Some(&project_path)).unwrap();

        assert_eq!(config.agent.as_deref(), Some("codex"));
        assert_eq!(config.container.image.as_deref(), Some("env-image"));
    }

    #[test]
    fn resolve_image_uses_agent_mapping() {
        let config = Config::default();
        assert_eq!(
            resolve_image(Some("claude"), &config),
            Some("ghcr.io/dstanek/am-claude-minimal:latest")
        );
        assert_eq!(
            resolve_image(Some("copilot"), &config),
            Some("ghcr.io/dstanek/am-copilot-minimal:latest")
        );
    }

    #[test]
    fn resolve_image_container_image_overrides_agent() {
        let mut config = Config::default();
        config.container.image = Some("custom-image:v1".to_string());
        // container.image takes priority over agent mapping
        assert_eq!(
            resolve_image(Some("claude"), &config),
            Some("custom-image:v1")
        );
    }

    #[test]
    fn resolve_image_returns_none_for_unknown_agent() {
        let config = Config::default();
        assert_eq!(resolve_image(Some("unknown-agent"), &config), None);
        assert_eq!(resolve_image(None, &config), None);
    }

    #[test]
    fn agent_image_overridden_in_project_config() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let project_path = write_toml(
            tmp.path(),
            "project.toml",
            r#"
[agents.claude]
image = "myorg/am-claude:custom"
"#,
        );

        let config = load_with_global(None, Some(&project_path)).unwrap();

        assert_eq!(
            config.agents.get("claude").and_then(|a| a.image.as_deref()),
            Some("myorg/am-claude:custom")
        );
        // copilot default is still present since project config didn't touch it
        assert_eq!(
            config
                .agents
                .get("copilot")
                .and_then(|a| a.image.as_deref()),
            Some("ghcr.io/dstanek/am-copilot-minimal:latest")
        );
    }

    #[test]
    fn agent_images_merged_across_global_and_project() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_AGENT"]);
        std::env::remove_var("AM_AGENT");
        let tmp = TempDir::new().unwrap();

        let global_path = write_toml(
            tmp.path(),
            "global.toml",
            r#"
[agents.gemini]
image = "myorg/am-gemini:latest"
"#,
        );

        let project_path = write_toml(
            tmp.path(),
            "project.toml",
            r#"
[agents.claude]
image = "myorg/am-claude:project"
"#,
        );

        let config = load_with_global(Some(&global_path), Some(&project_path)).unwrap();

        // Global added gemini
        assert_eq!(
            config.agents.get("gemini").and_then(|a| a.image.as_deref()),
            Some("myorg/am-gemini:latest")
        );
        // Project overrode claude
        assert_eq!(
            config.agents.get("claude").and_then(|a| a.image.as_deref()),
            Some("myorg/am-claude:project")
        );
        // Compiled-in copilot default still present
        assert_eq!(
            config
                .agents
                .get("copilot")
                .and_then(|a| a.image.as_deref()),
            Some("ghcr.io/dstanek/am-copilot-minimal:latest")
        );
    }

    #[test]
    fn global_state_dir_uses_xdg_state_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        let xdg_dir = tmp.path().join("xdg_state");
        std::env::set_var("XDG_STATE_HOME", &xdg_dir);
        std::env::remove_var("HOME");

        let path = global_state_dir();
        assert_eq!(path, Some(xdg_dir.join("am")));
    }

    #[test]
    fn global_state_dir_falls_back_to_home_local_state() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", tmp.path());

        let path = global_state_dir();
        assert_eq!(
            path,
            Some(tmp.path().join(".local").join("state").join("am"))
        );
    }

    #[test]
    fn global_state_dir_returns_none_without_home_or_xdg() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_STATE_HOME", "HOME"]);
        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("HOME");

        let path = global_state_dir();
        assert_eq!(path, None);
    }

    #[test]
    fn global_config_path_uses_xdg_config_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_CONFIG_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        let xdg_dir = tmp.path().join("xdg");
        std::env::set_var("XDG_CONFIG_HOME", &xdg_dir);
        std::env::remove_var("HOME");

        let path = global_config_path();
        assert_eq!(path, Some(xdg_dir.join("am").join("config.toml")));
    }

    #[test]
    fn global_config_path_falls_back_to_home_dot_config() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["XDG_CONFIG_HOME", "HOME"]);
        let tmp = TempDir::new().unwrap();
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", tmp.path());

        let path = global_config_path();
        assert_eq!(
            path,
            Some(tmp.path().join(".config").join("am").join("config.toml"))
        );
    }

    // ── validate_env_passthrough ──────────────────────────────────────────────

    #[test]
    fn valid_env_entries_accepted() {
        assert!(validate_env_passthrough(&[
            "ANTHROPIC_API_KEY".to_string(),
            "FOO=bar".to_string(),
            "_UNDERSCORE_START".to_string(),
            "LOWER_case=value".to_string(),
        ])
        .is_ok());
    }

    #[test]
    fn env_entry_starting_with_dash_rejected() {
        let err = validate_env_passthrough(&["--rm".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--rm"));
    }

    #[test]
    fn env_entry_with_space_in_name_rejected() {
        let err = validate_env_passthrough(&["FOO BAR=val".to_string()]).unwrap_err();
        assert!(err.to_string().contains("FOO BAR=val"));
    }

    #[test]
    fn env_entry_starting_with_digit_rejected() {
        let err = validate_env_passthrough(&["1INVALID".to_string()]).unwrap_err();
        assert!(err.to_string().contains("1INVALID"));
    }

    #[test]
    fn load_with_global_errors_on_bad_env_entry() {
        let tmp = TempDir::new().unwrap();
        let project = write_toml(
            tmp.path(),
            "config.toml",
            r#"
[container]
env = ["--rm"]
"#,
        );
        let err = load_with_global(None, Some(&project)).unwrap_err();
        assert!(err.to_string().contains("--rm"));
    }

    // ── split_percent validation ───────────────────────────────────────────────

    #[test]
    fn split_percent_out_of_range_in_toml_fails() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::remove_var("AM_TMUX_SPLIT_PERCENT");
        let tmp = TempDir::new().unwrap();
        let project = write_toml(
            tmp.path(),
            "config.toml",
            r#"
[tmux]
split_percent = 100
"#,
        );
        let err = load_with_global(None, Some(&project)).unwrap_err();
        assert!(
            err.to_string().contains("split_percent"),
            "expected split_percent error, got: {err}"
        );
    }

    #[test]
    fn split_percent_zero_in_toml_fails() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::remove_var("AM_TMUX_SPLIT_PERCENT");
        let tmp = TempDir::new().unwrap();
        let project = write_toml(
            tmp.path(),
            "config.toml",
            r#"
[tmux]
split_percent = 0
"#,
        );
        assert!(load_with_global(None, Some(&project)).is_err());
    }

    #[test]
    fn split_percent_env_var_out_of_range_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::set_var("AM_TMUX_SPLIT_PERCENT", "0");

        let err = load_with_global(None, None).unwrap_err();
        assert!(
            err.to_string().contains("AM_TMUX_SPLIT_PERCENT"),
            "expected AM_TMUX_SPLIT_PERCENT error, got: {err}"
        );
    }

    #[test]
    fn split_percent_env_var_over_max_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::set_var("AM_TMUX_SPLIT_PERCENT", "100");

        let err = load_with_global(None, None).unwrap_err();
        assert!(err.to_string().contains("AM_TMUX_SPLIT_PERCENT"));
    }

    #[test]
    fn split_percent_env_var_non_numeric_errors() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::set_var("AM_TMUX_SPLIT_PERCENT", "fifty");

        let err = load_with_global(None, None).unwrap_err();
        assert!(err.to_string().contains("AM_TMUX_SPLIT_PERCENT"));
    }

    #[test]
    fn split_percent_env_var_valid_value_applied() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_TMUX_SPLIT_PERCENT"]);
        std::env::set_var("AM_TMUX_SPLIT_PERCENT", "30");

        let config = load_with_global(None, None).unwrap();
        assert_eq!(config.tmux.split_percent, 30);
    }

    // ── validate_container_user ────────────────────────────────────────────────

    #[test]
    fn valid_container_users_accepted() {
        assert!(validate_container_user("am").is_ok());
        assert!(validate_container_user("_svc").is_ok());
        assert!(validate_container_user("dev-user1").is_ok());
    }

    #[test]
    fn container_user_with_path_traversal_rejected() {
        let err = validate_container_user("../root").unwrap_err();
        assert!(err.to_string().contains("../root"));
    }

    #[test]
    fn load_with_global_errors_on_invalid_container_user_in_file() {
        let tmp = TempDir::new().unwrap();
        let project = write_toml(
            tmp.path(),
            "config.toml",
            r#"
[container]
user = "../root"
"#,
        );
        let err = load_with_global(None, Some(&project)).unwrap_err();
        assert!(err.to_string().contains("../root"));
    }

    #[test]
    fn env_var_can_override_container_user() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_USER"]);
        std::env::set_var("AM_CONTAINER_USER", "dev-user1");

        let config = load_with_global(None, None).unwrap();

        assert_eq!(config.container.user, "dev-user1");
    }

    #[test]
    fn load_with_global_errors_on_invalid_container_user_in_env() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_USER"]);
        std::env::set_var("AM_CONTAINER_USER", "../root");

        let err = load_with_global(None, None).unwrap_err();

        assert!(err.to_string().contains("../root"));
    }

    // ── Devcontainer settings ─────────────────────────────────────────────────

    #[test]
    fn container_mode_defaults_to_auto() {
        // A repo that describes its environment in .devcontainer/ means for that
        // description to be used; preferring an am-specific image over it is the
        // surprising behaviour. Repos without a config fall back to an image.
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_MODE"]);
        std::env::remove_var("AM_CONTAINER_MODE");

        let config = load_with_global(None, None).unwrap();

        assert_eq!(config.container.mode, ContainerMode::Auto);
    }

    #[test]
    fn explicit_image_mode_still_overrides_the_default() {
        // The escape hatch for a repo whose devcontainer am cannot use.
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_MODE"]);
        std::env::remove_var("AM_CONTAINER_MODE");
        let tmp = TempDir::new().unwrap();
        let path = write_toml(tmp.path(), "project.toml", "[container]\nmode = \"image\"\n");

        let config = load_with_global(None, Some(&path)).unwrap();

        assert_eq!(config.container.mode, ContainerMode::Image);
    }

    #[test]
    fn container_mode_reads_from_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_MODE"]);
        std::env::remove_var("AM_CONTAINER_MODE");
        let tmp = TempDir::new().unwrap();
        let path = write_toml(
            tmp.path(),
            "project.toml",
            "[container]\nmode = \"devcontainer\"\n",
        );

        let config = load_with_global(None, Some(&path)).unwrap();

        assert_eq!(config.container.mode, ContainerMode::Devcontainer);
    }

    #[test]
    fn container_mode_env_overrides_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_CONTAINER_MODE"]);
        let tmp = TempDir::new().unwrap();
        let path = write_toml(tmp.path(), "project.toml", "[container]\nmode = \"image\"\n");
        std::env::set_var("AM_CONTAINER_MODE", "auto");

        let config = load_with_global(None, Some(&path)).unwrap();

        assert_eq!(config.container.mode, ContainerMode::Auto);
    }

    #[test]
    fn devcontainer_defaults_are_conservative() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&["AM_DEVCONTAINER_ALLOW_HOST_COMMANDS"]);
        std::env::remove_var("AM_DEVCONTAINER_ALLOW_HOST_COMMANDS");

        let config = load_with_global(None, None).unwrap();

        assert!(!config.devcontainer.allow_host_commands);
        assert!(!config.devcontainer.skip_lifecycle);
        assert_eq!(config.devcontainer.agent_install, AgentInstall::Auto);
        assert_eq!(config.devcontainer.cli, "devcontainer");
    }

    #[test]
    fn devcontainer_section_reads_from_file() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::save(&[
            "AM_DEVCONTAINER_PATH",
            "AM_DEVCONTAINER_AGENT_INSTALL",
            "AM_DEVCONTAINER_ALLOW_HOST_COMMANDS",
        ]);
        std::env::remove_var("AM_DEVCONTAINER_PATH");
        std::env::remove_var("AM_DEVCONTAINER_AGENT_INSTALL");
        std::env::remove_var("AM_DEVCONTAINER_ALLOW_HOST_COMMANDS");
        let tmp = TempDir::new().unwrap();
        let path = write_toml(
            tmp.path(),
            "project.toml",
            "[devcontainer]\n\
             path = \".devcontainer/custom.json\"\n\
             agent_install = \"bootstrap\"\n\
             allow_host_commands = true\n",
        );

        let config = load_with_global(None, Some(&path)).unwrap();

        assert_eq!(
            config.devcontainer.path.as_deref(),
            Some(Path::new(".devcontainer/custom.json"))
        );
        assert_eq!(config.devcontainer.agent_install, AgentInstall::Bootstrap);
        assert!(config.devcontainer.allow_host_commands);
    }

    #[test]
    fn extra_features_merge_rather_than_replace() {
        // A project adding one Feature should not have to restate the global set.
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let global = write_toml(
            tmp.path(),
            "global.toml",
            "[devcontainer.extra_features]\n\"ghcr.io/x/a:1\" = \"{}\"\n",
        );
        let project = write_toml(
            tmp.path(),
            "project.toml",
            "[devcontainer.extra_features]\n\"ghcr.io/x/b:1\" = \"{}\"\n",
        );

        let config = load_with_global(Some(&global), Some(&project)).unwrap();

        assert_eq!(config.devcontainer.extra_features.len(), 2);
    }

    #[test]
    fn claude_maps_to_the_official_devcontainer_feature() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let config = load_with_global(None, None).unwrap();

        assert_eq!(
            resolve_agent_feature(Some("claude"), &config),
            Some("ghcr.io/anthropics/devcontainer-features/claude-code:1")
        );
    }

    #[test]
    fn agents_without_a_published_feature_map_to_none() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let config = load_with_global(None, None).unwrap();

        assert_eq!(resolve_agent_feature(Some("copilot"), &config), None);
        assert_eq!(resolve_agent_feature(Some("gemini"), &config), None);
        assert_eq!(resolve_agent_feature(None, &config), None);
    }

    #[test]
    fn devcontainer_feature_can_be_overridden_per_agent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let path = write_toml(
            tmp.path(),
            "project.toml",
            "[agents.gemini]\ndevcontainer_feature = \"ghcr.io/me/gemini:1\"\n",
        );

        let config = load_with_global(None, Some(&path)).unwrap();

        assert_eq!(
            resolve_agent_feature(Some("gemini"), &config),
            Some("ghcr.io/me/gemini:1")
        );
    }
}
