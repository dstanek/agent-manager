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

// ── Config structs ────────────────────────────────────────────────────────────

/// Per-agent configuration (image override, etc.)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentSettings {
    pub image: Option<String>,
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
    pub runtime: RuntimePreference,
    pub image: Option<String>,
    pub agent: Option<String>,
    pub network: NetworkMode,
    pub env: Vec<String>,
    pub gitconfig: Option<PathBuf>, // None = ~/.gitconfig
    pub ssh: Option<PathBuf>,       // None = ~/.ssh
    pub user: String,               // container username (default: "am")
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            runtime: RuntimePreference::Auto,
            image: None,
            agent: None,
            network: NetworkMode::Full,
            env: Vec::new(),
            gitconfig: None,
            ssh: None,
            user: "am".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub vcs: Vcs,
    pub agent: Option<String>,
    /// Per-agent settings (image, etc.). Compiled-in defaults for known agents.
    pub agents: HashMap<String, AgentSettings>,
    pub tmux: TmuxConfig,
    pub container: ContainerConfig,
}

fn default_agent_images() -> HashMap<String, AgentSettings> {
    [
        ("claude", "ghcr.io/dstanek/am-claude-minimal:latest"),
        ("copilot", "ghcr.io/dstanek/am-copilot-minimal:latest"),
    ]
    .into_iter()
    .map(|(k, v)| {
        (
            k.to_string(),
            AgentSettings {
                image: Some(v.to_string()),
            },
        )
    })
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vcs: Vcs::Git,
            agent: None,
            agents: default_agent_images(),
            tmux: TmuxConfig::default(),
            container: ContainerConfig::default(),
        }
    }
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
    vcs: Option<Vcs>,
    agent: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileAgentSettings {
    image: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileTmux {
    agent_pane: Option<PaneSide>,
    split: Option<SplitDirection>,
    split_percent: Option<u8>,
}

#[derive(Debug, Deserialize, Default)]
struct FileContainer {
    enabled: Option<bool>,
    runtime: Option<RuntimePreference>,
    image: Option<String>,
    agent: Option<String>,
    network: Option<NetworkMode>,
    env: Option<Vec<String>>,
    gitconfig: Option<PathBuf>,
    ssh: Option<PathBuf>,
    user: Option<String>,
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
    apply_opt(&mut base.vcs, file.defaults.vcs);
    apply_opt_string(&mut base.agent, file.defaults.agent);

    // Merge agents: file entries extend/override the compiled-in defaults.
    for (name, file_agent) in file.agents {
        let entry = base.agents.entry(name).or_default();
        apply_opt_string(&mut entry.image, file_agent.image);
    }

    apply_opt(&mut base.tmux.agent_pane, file.tmux.agent_pane);
    apply_opt(&mut base.tmux.split, file.tmux.split);
    apply_opt(&mut base.tmux.split_percent, file.tmux.split_percent);

    apply_opt(&mut base.container.enabled, file.container.enabled);
    apply_opt(&mut base.container.runtime, file.container.runtime);
    apply_opt_string(&mut base.container.image, file.container.image);
    apply_opt_string(&mut base.container.agent, file.container.agent);
    apply_opt(&mut base.container.network, file.container.network);
    apply_opt(&mut base.container.env, file.container.env);
    apply_opt_some(&mut base.container.gitconfig, file.container.gitconfig);
    apply_opt_some(&mut base.container.ssh, file.container.ssh);
    if let Some(u) = file.container.user {
        if !u.is_empty() {
            base.container.user = u;
        }
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

/// Write the default project config file at `path` (creates parent directories as needed).
/// The file is written as a fully-commented-out template so it never silently overrides
/// global or compiled-in defaults.
pub fn write_defaults(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = r#"# Project-level am configuration — .am/config.toml
# Uncomment only the values you want to override from your global or compiled-in defaults.
# Precedence (highest wins): CLI flags > environment variables > project config > global config
# Run `am generate-config` to see the full global config template with all options documented.

[defaults]
# vcs = "git"            # "git" | "jj"
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
# runtime = "auto"       # "auto" | "podman" | "docker"
# network = "full"       # "full" | "none"
# env = []               # extra environment variables to pass into the container
# gitconfig = ""         # path to gitconfig to mount (default: ~/.gitconfig)
# ssh = ""               # path to SSH dir to mount (default: ~/.ssh)
# image = ""             # override image for all agents (advanced; prefer [agents.<name>].image)
# user = "am"            # username inside the container (used for credential mount paths)
"#;
    std::fs::write(path, content)?;
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
#   AM_VCS, AM_AGENT
#   AM_TMUX_AGENT_PANE, AM_TMUX_SPLIT, AM_TMUX_SPLIT_PERCENT
#   AM_CONTAINER_ENABLED, AM_CONTAINER_RUNTIME, AM_CONTAINER_IMAGE,
#   AM_CONTAINER_AGENT, AM_CONTAINER_NETWORK, AM_CONTAINER_USER

[defaults]
vcs = "git"            # "git" | "jj"
agent = "claude"       # agent to launch; also selects the container image via [agents.<name>]

# Per-agent image configuration. These are the compiled-in defaults — override here if needed.
[agents.claude]
image = "ghcr.io/dstanek/am-claude-minimal:latest"

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
runtime = "auto"       # "auto" (podman first, then docker) | "podman" | "docker"
network = "full"       # "full" (unrestricted) | "none" (no network access)
env = []               # extra environment variables passed into the container, e.g. ["FOO=bar"]
# gitconfig = ""        # path to gitconfig to mount (default: ~/.gitconfig)
# ssh = ""              # path to SSH dir to mount (default: ~/.ssh)
# image = ""            # override image for all agents (advanced; prefer [agents.<name>].image above)
# user = "am"           # username inside the container (used for credential mount paths)
"#
}

/// Read environment variables and apply them to the config.
///
/// Unknown/unrecognised values for enum fields are silently ignored so that
/// adding new enum variants is backwards-compatible. Numeric fields with hard
/// physical constraints (e.g. split_percent must be 1–99) return an error
/// rather than silently falling back, matching the behaviour of TOML parsing.
fn apply_env_vars(config: &mut Config) -> Result<()> {
    if let Ok(val) = std::env::var("AM_VCS") {
        match val.as_str() {
            "git" => config.vcs = Vcs::Git,
            "jj" => config.vcs = Vcs::Jj,
            _ => {}
        }
    }
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
    if let Ok(val) = std::env::var("AM_CONTAINER_AGENT") {
        if !val.is_empty() {
            config.container.agent = Some(val);
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
    if let Ok(val) = std::env::var("AM_CONTAINER_USER") {
        if !val.is_empty() {
            config.container.user = val;
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
            apply_file_config(&mut config, file);
        }
    }

    // Apply project config if provided and exists
    if let Some(path) = project_config_path {
        if path.exists() {
            let file = parse_config_file(path)?;
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

        assert_eq!(config.vcs, Vcs::Git);
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
            Self(
                keys.iter()
                    .map(|k| (*k, std::env::var_os(k)))
                    .collect(),
            )
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
}
