//! Agent harnesses — what to run, where it runs, and what credentials it needs.
//!
//! Historically `--agent` conflated three things: the command to launch, the environment it
//! launches in (image / devcontainer), and the credential integration that makes it work.
//! This module owns the first and third — the environment is still resolved through
//! [`crate::config`] via `resolve_image`/`resolve_agent_feature` (see the note on
//! [`builtin`]). "Harness" is the internal name for the bundle of command + integration this
//! module resolves; the user-facing vocabulary stays "agent" throughout (`--agent`,
//! `[agents.<name>]`, `defaults.agent`, `AM_AGENT`) — nothing here renames that surface.
//!
//! The four built-ins are values of the same types a user-defined agent produces in
//! `[agents.<name>]`, so nothing about `claude` is privileged over anything a config file can
//! describe. That is the whole point: before this, every one of these decisions was a `match`
//! arm on a closed enum, and there was no path to "run this image, mount these credentials,
//! exec this command" for anything outside the compiled-in four.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::container::MountMode;
use crate::error::AmError;

/// Where a credential lives on the host.
///
/// Not a plain `PathBuf`, because two of the built-ins need more than a fixed path: Claude
/// honours `CLAUDE_CONFIG_DIR`, and every entry is relative to a `$HOME` that is resolved
/// when the harness is *used*, not when it is defined.
#[derive(Debug, Clone, PartialEq)]
pub enum HostPath {
    /// `$HOME` joined with these components.
    UnderHome(Vec<String>),
    /// An environment variable naming an absolute path, falling back to `$HOME` joined with
    /// the components when it is unset or empty.
    EnvOrUnderHome { var: String, fallback: Vec<String> },
    /// A path that does not depend on `$HOME`. Only reachable from config: no built-in keeps
    /// credentials outside the home directory.
    Absolute(PathBuf),
}

impl HostPath {
    pub fn resolve(&self) -> Result<PathBuf> {
        match self {
            HostPath::UnderHome(parts) => Ok(join_home(home_dir()?, parts)),
            HostPath::EnvOrUnderHome { var, fallback } => {
                match std::env::var(var).ok().filter(|v| !v.is_empty()) {
                    Some(value) => Ok(PathBuf::from(value)),
                    None => Ok(join_home(home_dir()?, fallback)),
                }
            }
            HostPath::Absolute(path) => Ok(path.clone()),
        }
    }
}

fn join_home(home: PathBuf, parts: &[String]) -> PathBuf {
    parts.iter().fold(home, |acc, part| acc.join(part))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME").map(PathBuf::from).with_context(|| {
        "HOME environment variable not set — cannot resolve user home directory for mounts"
    })
}

/// One condition that can make an agent authenticated.
#[derive(Debug, Clone, PartialEq)]
pub enum Requirement {
    PathExists(HostPath),
    EnvSet(String),
}

impl Requirement {
    fn satisfied(&self) -> bool {
        match self {
            Requirement::PathExists(path) => path.resolve().map(|p| p.exists()).unwrap_or(false),
            Requirement::EnvSet(var) => std::env::var(var)
                .ok()
                .is_some_and(|value| !value.trim().is_empty()),
        }
    }
}

/// A host credential path made visible inside the container.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialMount {
    pub host: HostPath,
    /// Path inside the container. Relative paths are resolved against the container's home
    /// directory, which is not the host's — a devcontainer's `remoteUser` may be `root`. An
    /// absolute path is used as given.
    pub container: String,
    pub mode: MountMode,
    /// Preflight fails when this path is missing. The rest are best-effort: Claude's
    /// `.claude.json` and Copilot's `github-copilot` directory are useful when present and
    /// not worth failing a session over when absent.
    pub required: bool,
    /// Skip the mount entirely when the host path does not exist, rather than letting the
    /// runtime create it root-owned on the host.
    pub only_if_exists: bool,
}

/// Where an environment variable's value comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvSource {
    /// Forwarded from the host environment when set and non-empty.
    Passthrough(String),
    /// The GitHub CLI's auth token, obtained by running `gh auth token`.
    ///
    /// The one credential no user-defined agent will be able to express as data, because it
    /// comes from running a command rather than reading a path or a variable. It stays a
    /// named variant rather than a general "run this command" escape hatch: shelling out
    /// during preflight is a capability worth granting deliberately, not by config.
    GhToken,
}

/// How an agent authenticates. `None` on a [`Harness`] is a first-class value — a command
/// that needs no credentials from the host is a perfectly good agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Integration {
    pub mounts: Vec<CredentialMount>,
    pub env: Vec<EnvSource>,
    /// OR of ANDs: at least one group must be fully satisfied. Only Codex needs the outer
    /// list to have more than one entry — an API key *or* an interactive sign-in — but
    /// expressing the other three as a single one-element group keeps one code path.
    pub requires_any: Vec<Vec<Requirement>>,
    /// Shown when `requires_any` has alternatives and none of them is satisfied. A
    /// single-group integration reports the specific missing path instead, which is more
    /// useful; with alternatives there is no single path to name.
    pub alternatives_message: Option<String>,
    /// How to obtain credentials. `am doctor`'s failure hint.
    pub hint: String,
    /// An unresolvable `$HOME` yields no mounts rather than an error. Only Codex, which can
    /// be authenticated by an environment variable alone.
    pub home_optional: bool,
}

impl Integration {
    /// Whether any alternative is satisfied. Presence only — this makes no claim about
    /// whether the credentials found are still *valid*.
    pub fn satisfied(&self) -> bool {
        self.requires_any
            .iter()
            .any(|group| group.iter().all(Requirement::satisfied))
    }

    /// The first requirement standing in the way, when there is exactly one group. With
    /// alternatives there is no single culprit and callers use `alternatives_message`.
    pub fn unsatisfied_path(&self) -> Option<PathBuf> {
        if self.requires_any.len() != 1 {
            return None;
        }
        self.requires_any[0].iter().find_map(|req| match req {
            Requirement::PathExists(path) => {
                path.resolve().ok().filter(|resolved| !resolved.exists())
            }
            Requirement::EnvSet(_) => None,
        })
    }
}

/// A named, fully-resolved agent harness: what `[agents.<name>]` (plus, for a built-in, the
/// compiled-in profile) resolves to. Does **not** carry `image`/`devcontainer_feature` —
/// those stay resolved separately via `config::resolve_image`/`resolve_agent_feature`, which
/// already read the same `[agents.<name>]` table; duplicating them here would create two
/// sources of truth for the one decision config already owns.
#[derive(Debug, Clone, PartialEq)]
pub struct Harness {
    pub name: String,
    /// argv. The first element is the binary.
    pub command: Vec<String>,
    /// Appended to `command` under `--auto`.
    pub auto_flags: Vec<String>,
    /// How to resume the previous conversation, or `None` for an agent confirmed not to
    /// support that. argv rather than flags, because Codex's form is a subcommand.
    pub resume: Option<Vec<String>>,
    pub integration: Option<Integration>,
}

fn s(value: &str) -> String {
    value.to_string()
}

fn parts(values: &[&str]) -> Vec<String> {
    values.iter().copied().map(s).collect()
}

/// The compiled-in agents.
pub fn builtin(name: &str) -> Option<Harness> {
    match name {
        "claude" => Some(Harness {
            name: s("claude"),
            command: parts(&["claude"]),
            auto_flags: parts(&["--dangerously-skip-permissions"]),
            resume: Some(parts(&["--continue"])),
            integration: Some(Integration {
                mounts: vec![
                    CredentialMount {
                        host: HostPath::EnvOrUnderHome {
                            var: s("CLAUDE_CONFIG_DIR"),
                            fallback: parts(&[".claude"]),
                        },
                        container: s(".claude"),
                        mode: MountMode::ReadWrite,
                        required: true,
                        only_if_exists: false,
                    },
                    CredentialMount {
                        host: HostPath::UnderHome(parts(&[".claude.json"])),
                        container: s(".claude.json"),
                        mode: MountMode::ReadWrite,
                        required: false,
                        only_if_exists: false,
                    },
                ],
                env: vec![],
                requires_any: vec![vec![Requirement::PathExists(HostPath::EnvOrUnderHome {
                    var: s("CLAUDE_CONFIG_DIR"),
                    fallback: parts(&[".claude"]),
                })]],
                alternatives_message: None,
                hint: s(
                    "run 'claude auth login' (or set ANTHROPIC_API_KEY) — see \
                     https://dstanek.github.io/agent-manager/guides/claude-code/#prerequisites",
                ),
                home_optional: false,
            }),
        }),

        "copilot" => Some(Harness {
            name: s("copilot"),
            command: parts(&["copilot"]),
            auto_flags: vec![],
            resume: Some(parts(&["--continue"])),
            integration: Some(Integration {
                mounts: vec![
                    CredentialMount {
                        // GitHub CLI auth token — required for Copilot authentication.
                        host: HostPath::UnderHome(parts(&[".config", "gh"])),
                        container: s(".config/gh"),
                        mode: MountMode::ReadOnly,
                        required: true,
                        only_if_exists: false,
                    },
                    CredentialMount {
                        host: HostPath::UnderHome(parts(&[".config", "github-copilot"])),
                        container: s(".config/github-copilot"),
                        mode: MountMode::ReadOnly,
                        required: false,
                        only_if_exists: false,
                    },
                ],
                env: vec![EnvSource::GhToken],
                requires_any: vec![vec![Requirement::PathExists(HostPath::UnderHome(parts(&[
                    ".config", "gh",
                ])))]],
                alternatives_message: None,
                hint: s(
                    "run 'gh auth login' — see \
                     https://dstanek.github.io/agent-manager/guides/github-copilot/#prerequisites",
                ),
                home_optional: false,
            }),
        }),

        "gemini" => Some(Harness {
            name: s("gemini"),
            command: parts(&["gemini"]),
            auto_flags: vec![],
            resume: Some(parts(&["--resume", "latest"])),
            integration: Some(Integration {
                mounts: vec![CredentialMount {
                    host: HostPath::UnderHome(parts(&[".gemini"])),
                    container: s(".gemini"),
                    mode: MountMode::ReadOnly,
                    required: true,
                    only_if_exists: false,
                }],
                env: vec![],
                requires_any: vec![vec![Requirement::PathExists(HostPath::UnderHome(parts(&[
                    ".gemini",
                ])))]],
                alternatives_message: None,
                hint: s(
                    "authenticate with the Gemini CLI on this host — see \
                     https://dstanek.github.io/agent-manager/guides/gemini/#prerequisites",
                ),
                home_optional: false,
            }),
        }),

        "codex" => Some(Harness {
            name: s("codex"),
            command: parts(&["codex"]),
            auto_flags: vec![],
            // A subcommand, not a flag — it has to come right after the binary name, giving
            // `codex resume --last`.
            resume: Some(parts(&["resume", "--last"])),
            integration: Some(Integration {
                mounts: vec![CredentialMount {
                    // The whole directory, read-write. Codex signs in interactively and
                    // rotates the token in auth.json, which it *replaces* rather than
                    // rewrites — a single-file mount would leave the container writing to a
                    // detached inode the host never sees, and read-only would work until the
                    // first token refresh and then fail.
                    host: HostPath::UnderHome(parts(&[".codex"])),
                    container: s(".codex"),
                    mode: MountMode::ReadWrite,
                    required: false,
                    only_if_exists: true,
                }],
                env: vec![EnvSource::Passthrough(s("OPENAI_API_KEY"))],
                // Two independent ways to be authenticated, either sufficient. Requiring the
                // key locked out every user who had signed in interactively.
                requires_any: vec![
                    vec![Requirement::PathExists(HostPath::UnderHome(parts(&[
                        ".codex", "auth.json",
                    ])))],
                    vec![Requirement::EnvSet(s("OPENAI_API_KEY"))],
                ],
                alternatives_message: Some(s(
                    "OPENAI_API_KEY is not set and ~/.codex does not exist\n\
                     Run 'codex' once to sign in, or export OPENAI_API_KEY=sk-...",
                )),
                hint: s(
                    "run 'codex' once to sign in (or set OPENAI_API_KEY) — see \
                     https://dstanek.github.io/agent-manager/guides/codex/#prerequisites",
                ),
                home_optional: true,
            }),
        }),

        _ => None,
    }
}

/// Every compiled-in agent name, in the order error messages and menus list them.
pub const BUILTIN_NAMES: &[&str] = &["claude", "copilot", "gemini", "codex"];

/// An `--agent`/`defaults.agent` name that has been checked against the table.
///
/// Replaces the old closed `KnownAgent` enum for identity/menu purposes — validating a name,
/// displaying it, or passing it around before it is actually resolved into a full
/// [`Harness`]. The distinction matters: an enum could only ever name agents the binary was
/// compiled with, so there was no path to a config-defined one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentName(String);

impl AgentName {
    /// Check a name against the full table: the compiled-in entries plus whatever
    /// `[agents.<name>]` defines. Used by `am start`/`am attach`.
    pub fn parse(value: &str, cfg: &Config) -> Result<Self> {
        if builtin(value).is_some() || cfg.agents.contains_key(value) {
            return Ok(AgentName(value.to_string()));
        }
        Err(unknown(value, cfg))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This name's full definition: the compiled-in one, with `[agents.<name>]` overlaid.
    pub fn resolve(&self, cfg: &Config) -> Result<Harness> {
        resolve(self.as_str(), cfg)
    }
}

impl std::fmt::Display for AgentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A plain `write!` of the bare name: callers pad this into aligned menus, and a
        // `Display` that added decoration would break their column arithmetic.
        write!(f, "{}", self.0)
    }
}

/// The agent offered when nothing else selects one.
pub fn default_name() -> AgentName {
    AgentName(BUILTIN_NAMES[0].to_string())
}

/// A built-in's name, for call sites that name one literally.
///
/// Panics on anything that is not compiled in. That is the point: the argument is always a
/// literal in the source, never user input, so a bad one is a bug rather than a condition to
/// handle. User input goes through [`AgentName::parse`].
#[cfg(test)]
pub fn builtin_name(name: &str) -> AgentName {
    AgentName::parse(name, &Config::default())
        .unwrap_or_else(|_| panic!("'{name}' is not a compiled-in agent"))
}

/// Every agent name available, built-ins first and config-only ones after, each group in its
/// own stable order — the list an error message or a menu shows.
pub fn all_names(cfg: &Config) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_NAMES.iter().map(|n| n.to_string()).collect();
    let mut extra: Vec<String> = cfg
        .agents
        .keys()
        .filter(|name| builtin(name).is_none())
        .cloned()
        .collect();
    extra.sort();
    names.extend(extra);
    names
}

/// Resolve `name` against the compiled-in profile (if any) and `[agents.<name>]` (if any).
///
/// Either half may be absent. A built-in with no config entry is used as-is; a config entry
/// for a name `am` was never compiled with is a complete definition on its own, which is the
/// whole point of the table being open. Fails when neither half exists, or when a
/// config-only entry has no `command` to fall back to.
pub fn resolve(name: &str, cfg: &Config) -> Result<Harness> {
    let builtin_profile = builtin(name);
    let settings = cfg.agents.get(name);

    let Some(settings) = settings else {
        return builtin_profile.ok_or_else(|| unknown(name, cfg));
    };

    let mut profile = builtin_profile.unwrap_or_else(|| Harness {
        name: name.to_string(),
        command: vec![],
        auto_flags: vec![],
        resume: None,
        integration: None,
    });

    if let Some(command) = &settings.command {
        profile.command = command.clone();
    }
    if let Some(auto_flags) = &settings.auto_flags {
        profile.auto_flags = auto_flags.clone();
    }
    if let Some(resume) = &settings.resume {
        profile.resume = Some(resume.clone());
    }
    if let Some(integration) = &settings.integration {
        profile.integration = Some(convert_integration(name, integration)?);
    }

    if profile.command.is_empty() {
        return Err(AmError::ConfigError(format!(
            "agent '{name}' is defined in config but has no command — add \
             `command = [\"...\"]` under [agents.{name}]"
        ))
        .into());
    }
    Ok(profile)
}

fn unknown(name: &str, cfg: &Config) -> anyhow::Error {
    AmError::ConfigError(format!(
        "unknown agent '{name}' — configured agents are: {}",
        all_names(cfg).join(", "),
    ))
    .into()
}

fn convert_integration(
    agent: &str,
    settings: &crate::config::IntegrationSettings,
) -> Result<Integration> {
    let mut mounts = Vec::new();
    for mount in &settings.mounts {
        let mode = match mount.mode.as_deref().unwrap_or("ro") {
            "ro" => MountMode::ReadOnly,
            "rw" => MountMode::ReadWrite,
            other => {
                return Err(AmError::ConfigError(format!(
                    "agent '{agent}': mount mode '{other}' is not valid — use \"ro\" or \"rw\""
                ))
                .into())
            }
        };
        mounts.push(CredentialMount {
            host: parse_host_path(agent, &mount.host)?,
            container: mount.container.trim_start_matches("~/").to_string(),
            mode,
            // A mount someone bothered to declare is presumed to matter.
            required: mount.required.unwrap_or(true),
            only_if_exists: mount.only_if_exists.unwrap_or(false),
        });
    }

    let mut requires_any = Vec::new();
    for group in &settings.requires_any {
        let mut converted = Vec::new();
        for requirement in group {
            match (&requirement.path, &requirement.env) {
                (Some(path), None) => {
                    converted.push(Requirement::PathExists(parse_host_path(agent, path)?))
                }
                (None, Some(var)) => converted.push(Requirement::EnvSet(var.clone())),
                _ => {
                    return Err(AmError::ConfigError(format!(
                        "agent '{agent}': each entry in requires_any needs exactly one of \
                         `path` or `env`"
                    ))
                    .into())
                }
            }
        }
        requires_any.push(converted);
    }

    Ok(Integration {
        mounts,
        env: settings
            .env
            .iter()
            .map(|var| EnvSource::Passthrough(var.clone()))
            .collect(),
        requires_any,
        // Only meaningful with more than one group, and a config-defined agent that has
        // alternatives gets the generic wording rather than a bespoke sentence.
        alternatives_message: (settings.requires_any.len() > 1)
            .then(|| s("none of its alternatives is present")),
        hint: settings.hint.clone().unwrap_or_default(),
        // Every config-defined credential is either an absolute path or under $HOME; an
        // unresolvable $HOME is a real failure rather than something to tolerate.
        home_optional: false,
    })
}

/// A host path from config: `~/...` relative to the user's home, or absolute.
///
/// `~` is expanded at *use* time rather than load time — `.am/config.toml` is meant to be
/// committed, so the home it names is the reader's, not the author's.
fn parse_host_path(agent: &str, value: &str) -> Result<HostPath> {
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(HostPath::UnderHome(
            rest.split('/').map(|part| part.to_string()).collect(),
        ));
    }
    if value.starts_with('/') {
        return Ok(HostPath::Absolute(PathBuf::from(value)));
    }
    Err(AmError::ConfigError(format!(
        "agent '{agent}': host path '{value}' must start with '~/' or '/' — a relative path \
         would resolve against whatever directory `am` happened to run in"
    ))
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, AgentSettings, IntegrationSettings, MountSettings, RequirementSettings};

    fn cfg_with(name: &str, settings: AgentSettings) -> Config {
        let mut cfg = Config::default();
        cfg.agents.insert(name.to_string(), settings);
        cfg
    }

    #[test]
    fn a_builtin_resolves_without_any_config() {
        let claude = resolve("claude", &Config::default()).unwrap();
        assert_eq!(claude.command, vec!["claude"]);
        assert_eq!(claude.auto_flags, vec!["--dangerously-skip-permissions"]);
        assert!(claude.integration.is_some());
    }

    #[test]
    fn a_config_only_agent_resolves_from_config_alone() {
        // The point of the whole feature: a name `am` has never heard of.
        let cfg = cfg_with(
            "aider",
            AgentSettings {
                command: Some(vec!["aider".to_string(), "--model".to_string()]),
                ..AgentSettings::default()
            },
        );
        let aider = resolve("aider", &cfg).unwrap();
        assert_eq!(aider.name, "aider");
        assert_eq!(aider.command, vec!["aider", "--model"]);
        // No preset is a first-class outcome, not a degraded one.
        assert!(aider.integration.is_none());
        assert!(aider.resume.is_none());
    }

    #[test]
    fn overriding_one_field_of_a_builtin_leaves_the_rest_alone() {
        let cfg = cfg_with(
            "claude",
            AgentSettings {
                auto_flags: Some(vec![]),
                ..AgentSettings::default()
            },
        );
        let claude = resolve("claude", &cfg).unwrap();
        assert!(claude.auto_flags.is_empty(), "the override must take effect");
        // Everything not mentioned still comes from the built-in.
        assert_eq!(claude.command, vec!["claude"]);
        assert_eq!(claude.resume, Some(vec!["--continue".to_string()]));
        assert!(claude.integration.is_some());
    }

    #[test]
    fn a_config_entry_with_no_command_and_no_builtin_is_an_error() {
        // Distinguishing "no such agent" from "defined but unusable" is the difference
        // between a typo the user can see and a silent nothing.
        let cfg = cfg_with("half-defined", AgentSettings::default());
        let err = resolve("half-defined", &cfg).unwrap_err().to_string();
        assert!(err.contains("has no command"), "{err}");
        assert!(err.contains("half-defined"), "{err}");
    }

    #[test]
    fn an_unknown_name_lists_what_is_configured() {
        let cfg = cfg_with(
            "aider",
            AgentSettings {
                command: Some(vec!["aider".to_string()]),
                ..AgentSettings::default()
            },
        );
        let err = resolve("nope", &cfg).unwrap_err().to_string();
        assert!(err.contains("claude"), "{err}");
        assert!(err.contains("aider"), "built-ins are not the whole list: {err}");
    }

    #[test]
    fn config_integrations_convert_to_the_same_shape_the_builtins_use() {
        let cfg = cfg_with(
            "aider",
            AgentSettings {
                command: Some(vec!["aider".to_string()]),
                integration: Some(IntegrationSettings {
                    mounts: vec![MountSettings {
                        host: "~/.aider.conf.yml".to_string(),
                        container: "~/.aider.conf.yml".to_string(),
                        mode: Some("rw".to_string()),
                        ..MountSettings::default()
                    }],
                    env: vec!["ANTHROPIC_API_KEY".to_string()],
                    requires_any: vec![vec![RequirementSettings {
                        env: Some("ANTHROPIC_API_KEY".to_string()),
                        path: None,
                    }]],
                    hint: Some("export ANTHROPIC_API_KEY".to_string()),
                }),
                ..AgentSettings::default()
            },
        );
        let integration = resolve("aider", &cfg).unwrap().integration.unwrap();
        assert_eq!(
            integration.mounts[0].host,
            HostPath::UnderHome(vec![".aider.conf.yml".to_string()])
        );
        // The leading `~/` is the *container's* home, which is not the host's, so it is
        // stripped and rejoined against the container home at mount time.
        assert_eq!(integration.mounts[0].container, ".aider.conf.yml");
        assert_eq!(integration.mounts[0].mode, MountMode::ReadWrite);
        assert!(integration.mounts[0].required, "mounts default to required");
        assert_eq!(
            integration.env,
            vec![EnvSource::Passthrough("ANTHROPIC_API_KEY".to_string())]
        );
        assert_eq!(integration.hint, "export ANTHROPIC_API_KEY");
    }

    #[test]
    fn an_absolute_host_path_is_allowed_and_a_relative_one_is_not() {
        let mount = |host: &str| AgentSettings {
            command: Some(vec!["x".to_string()]),
            integration: Some(IntegrationSettings {
                mounts: vec![MountSettings {
                    host: host.to_string(),
                    container: "creds".to_string(),
                    ..MountSettings::default()
                }],
                ..IntegrationSettings::default()
            }),
            ..AgentSettings::default()
        };
        assert!(resolve("x", &cfg_with("x", mount("/etc/creds"))).is_ok());

        // A relative path would resolve against whatever directory `am` ran in, which is
        // never what anyone means and fails differently depending on where you invoked it.
        let err = resolve("x", &cfg_with("x", mount("creds/here")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must start with"), "{err}");
    }

    #[test]
    fn a_nonsense_mount_mode_is_rejected_at_resolve_time() {
        let cfg = cfg_with(
            "x",
            AgentSettings {
                command: Some(vec!["x".to_string()]),
                integration: Some(IntegrationSettings {
                    mounts: vec![MountSettings {
                        host: "/a".to_string(),
                        container: "b".to_string(),
                        mode: Some("read-write".to_string()),
                        ..MountSettings::default()
                    }],
                    ..IntegrationSettings::default()
                }),
                ..AgentSettings::default()
            },
        );
        let err = resolve("x", &cfg).unwrap_err().to_string();
        assert!(err.contains("read-write"), "{err}");
        assert!(err.contains("\"ro\""), "{err}");
    }

    #[test]
    fn a_requirement_naming_both_path_and_env_is_rejected() {
        let cfg = cfg_with(
            "x",
            AgentSettings {
                command: Some(vec!["x".to_string()]),
                integration: Some(IntegrationSettings {
                    requires_any: vec![vec![RequirementSettings {
                        path: Some("~/a".to_string()),
                        env: Some("B".to_string()),
                    }]],
                    ..IntegrationSettings::default()
                }),
                ..AgentSettings::default()
            },
        );
        let err = resolve("x", &cfg).unwrap_err().to_string();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn all_names_puts_builtins_first_and_sorts_the_rest() {
        let mut cfg = Config::default();
        for name in ["zebra", "aider"] {
            cfg.agents.insert(
                name.to_string(),
                AgentSettings {
                    command: Some(vec![name.to_string()]),
                    ..AgentSettings::default()
                },
            );
        }
        assert_eq!(
            all_names(&cfg),
            vec!["claude", "copilot", "gemini", "codex", "aider", "zebra"]
        );
    }
}
