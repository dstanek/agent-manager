//! `am setup` — the guided front door.
//!
//! Everything specific to the question flow lives here: what `am` can work out on its own
//! (`DetectedState`), the two questions it cannot (`ask_agent`, `ask_container_enabled`),
//! and the two config writes those answers can produce.
//!
//! Two rules shape the module:
//!
//! - **Ask only what detected state cannot answer.** Every prompt shows the *effective*
//!   current value and where it comes from, and accepting it is a guaranteed no-op —
//!   `update_project_agent` / `update_global_container_enabled` return `Ok(false)` without
//!   touching the file at all. That is what makes a second `am setup` run silent.
//! - **Credentials are probed for presence only.** Nothing here reads, prints, or writes a
//!   secret; `agent_credentials` holds the same booleans `am doctor` already derives.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::RuntimePreference;
use crate::{config, container, devcontainer, tmux};

/// The agents the menu offers, in the order shown. `KnownAgent` is the source of truth for
/// which agents exist; this list only fixes their order.
const MENU: [container::KnownAgent; 4] = [
    container::KnownAgent::Claude,
    container::KnownAgent::Copilot,
    container::KnownAgent::Gemini,
    container::KnownAgent::Codex,
];

// ── Effective values, and where they come from ────────────────────────────────

/// Where an effective value currently comes from.
///
/// Used to label prompts and to pick their default. It never decides the *write* target:
/// `defaults.agent` always goes to the project file and `container.enabled` always goes to
/// the global one, however the current value was inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Project,
    Global,
    CompiledDefault,
}

impl Source {
    /// How the source reads inside a prompt.
    pub fn label(self) -> &'static str {
        match self {
            Source::Project => "from this project's config",
            Source::Global => "from your global config",
            Source::CompiledDefault => "am's default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective<T> {
    pub value: T,
    pub source: Source,
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// What `am setup` already knows without asking, gathered once up front.
///
/// Mirrors the inputs `doctor::run` and `cmd_start` use, so a question is never asked about
/// something those functions could answer themselves.
#[derive(Debug, Clone)]
pub struct DetectedState {
    /// `None` when the current directory is not inside a repository.
    pub vcs: Option<config::Vcs>,
    pub project_config_path: PathBuf,
    pub project_config_exists: bool,
    /// `None` only when neither `XDG_CONFIG_HOME` nor `HOME` is set.
    pub global_config_path: Option<PathBuf>,
    pub global_config_exists: bool,
    pub tmux_present: bool,
    /// Empty, one, or both — the containers question turns on this being empty.
    pub runtimes_found: Vec<container::RuntimeKind>,
    pub devcontainer: Option<PathBuf>,
    /// Presence only. No credential is ever read, displayed, or written.
    pub agent_credentials: Vec<(container::KnownAgent, bool)>,
    pub effective_agent: Effective<Option<container::KnownAgent>>,
    pub effective_container_enabled: Effective<bool>,
}

impl DetectedState {
    /// Probe the host and both config files.
    ///
    /// Takes the repository the same way `doctor::run` does — the caller has already
    /// resolved it, and re-deriving the VCS here would be a second answer to a question
    /// that already has one.
    pub fn gather(repo: Option<(&Path, config::Vcs)>) -> Result<Self> {
        let repo_root = repo.as_ref().map(|(root, _)| *root);
        let vcs = repo.map(|(_, vcs)| vcs);

        let project_config_path = repo_root
            .unwrap_or_else(|| Path::new(""))
            .join(".am")
            .join("config.toml");
        let project_config_exists = project_config_path.is_file();
        let global_config_path = config::global_config_path();
        let global_config_exists = global_config_path.as_deref().is_some_and(Path::is_file);

        let (effective_agent, effective_container_enabled) = resolve_effective(
            project_config_exists.then_some(project_config_path.as_path()),
            global_config_path.as_deref().filter(|p| p.is_file()),
        );

        // Asked per runtime rather than via RuntimePreference::Auto: the question is which
        // runtimes exist, not which one a session would pick.
        let runtimes_found = [
            (RuntimePreference::Podman, container::RuntimeKind::Podman),
            (RuntimePreference::Docker, container::RuntimeKind::Docker),
        ]
        .into_iter()
        .filter(|(preference, _)| container::detect_runtime(preference.clone()).is_ok())
        .map(|(_, kind)| kind)
        .collect();

        // A discovery error (two configs, an unreadable one) is doctor's to report; here it
        // only means "nothing to say about a devcontainer".
        let devcontainer = repo_root
            .and_then(|root| devcontainer::find_config(root, None).ok())
            .flatten();

        let agent_credentials = MENU
            .iter()
            .map(|agent| {
                (
                    *agent,
                    container::validate_agent_credentials(*agent).is_ok(),
                )
            })
            .collect();

        Ok(Self {
            vcs,
            project_config_path,
            project_config_exists,
            global_config_path,
            global_config_exists,
            tmux_present: tmux::find_tmux().is_some(),
            runtimes_found,
            devcontainer,
            agent_credentials,
            effective_agent,
            effective_container_enabled,
        })
    }

    fn has_credentials(&self, agent: container::KnownAgent) -> bool {
        self.agent_credentials
            .iter()
            .any(|(known, present)| *known == agent && *present)
    }

    /// The agent a prompt pre-selects: whatever is already configured, else the first agent
    /// already authenticated on this host, else claude.
    fn default_agent(&self) -> container::KnownAgent {
        self.effective_agent
            .value
            .or_else(|| {
                self.agent_credentials
                    .iter()
                    .find(|(_, present)| *present)
                    .map(|(agent, _)| *agent)
            })
            .unwrap_or(container::KnownAgent::Claude)
    }
}

/// The two keys `am setup` tracks, read from one file on its own.
///
/// Deliberately not `config::load_with_global`: that merges the layers into a single answer,
/// and knowing *which* layer an answer came from is the whole point here.
#[derive(Debug, Default)]
struct TrackedKeys {
    defaults: TrackedDefaults,
    container: TrackedContainer,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackedDefaults {
    agent: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackedContainer {
    enabled: Option<bool>,
}

/// Read the tracked keys out of one config file. A missing or unparseable file reads as
/// "sets nothing" — `doctor::run` is what reports a broken config, and it runs a few steps
/// later against the same file.
///
/// `defaults` and `container` are deserialized independently rather than as one
/// `toml::from_str::<TrackedKeys>` call: that call fails the whole read the moment either
/// sub-table is malformed, which would let a bad `defaults.agent` mask a perfectly
/// well-formed `container.enabled` in the same file (and vice versa) — turning one broken
/// key into two questions asked without cause.
fn read_tracked(path: Option<&Path>) -> TrackedKeys {
    let Some(path) = path else {
        return TrackedKeys::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return TrackedKeys::default();
    };
    let Ok(root) = text.parse::<toml::Table>() else {
        return TrackedKeys::default();
    };

    let defaults = root
        .get("defaults")
        .cloned()
        .and_then(|value| value.try_into::<TrackedDefaults>().ok())
        .unwrap_or_default();
    let container = root
        .get("container")
        .cloned()
        .and_then(|value| value.try_into::<TrackedContainer>().ok())
        .unwrap_or_default();

    TrackedKeys { defaults, container }
}

/// Resolve both tracked keys with project → global → compiled-default precedence, keeping
/// the layer each answer came from.
fn resolve_effective(
    project: Option<&Path>,
    global: Option<&Path>,
) -> (
    Effective<Option<container::KnownAgent>>,
    Effective<bool>,
) {
    let project = read_tracked(project);
    let global = read_tracked(global);

    // The source is the first layer that *sets* the key, even if it sets it to a name that
    // is not a known agent: that value is still what the file says, doctor still reports it,
    // and treating the slot as unfilled here is what lets `am setup` repair it (UC3).
    let agent = match (project.defaults.agent, global.defaults.agent) {
        (Some(name), _) => Effective {
            value: container::KnownAgent::parse(&name).ok(),
            source: Source::Project,
        },
        (None, Some(name)) => Effective {
            value: container::KnownAgent::parse(&name).ok(),
            source: Source::Global,
        },
        (None, None) => Effective {
            value: None,
            source: Source::CompiledDefault,
        },
    };

    let enabled = match (project.container.enabled, global.container.enabled) {
        (Some(value), _) => Effective {
            value,
            source: Source::Project,
        },
        (None, Some(value)) => Effective {
            value,
            source: Source::Global,
        },
        (None, None) => Effective {
            value: config::ContainerConfig::default().enabled,
            source: Source::CompiledDefault,
        },
    };

    (agent, enabled)
}

// ── The IO seam ───────────────────────────────────────────────────────────────

/// Where questions are asked and answered.
///
/// A trait rather than direct `stdin`/`stdout` use so the question logic — defaults,
/// re-prompting, end of input — is unit-testable without a subprocess or a real TTY.
pub trait Io {
    /// Ask, and return the answer with surrounding whitespace trimmed. `None` means end of
    /// input: the caller aborts rather than looping on a stream that will never answer.
    fn prompt_line(&mut self, question: &str) -> Option<String>;
    fn println(&mut self, line: &str);
}

/// The real terminal. The caller gates its use on stdin being a TTY.
pub struct TermIo;

impl Io for TermIo {
    fn prompt_line(&mut self, question: &str) -> Option<String> {
        use std::io::Write as _;
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(input.trim().to_string()),
        }
    }

    fn println(&mut self, line: &str) {
        println!("{line}");
    }
}

/// Stdin closed mid-flow. One message, and the caller stops — every remaining step needs an
/// answer that is not coming.
fn eof_aborted() -> anyhow::Error {
    anyhow::anyhow!("no input received; re-run with --yes for non-interactive setup")
}

// ── Question 4: which agent ───────────────────────────────────────────────────

/// The write implied by choosing `chosen`, or `None` when the config already resolves that
/// way. A `Some` is always written to the *project* file, whatever layer the current value
/// came from.
fn agent_write(
    detected: &DetectedState,
    chosen: container::KnownAgent,
) -> Option<container::KnownAgent> {
    (detected.effective_agent.value != Some(chosen)).then_some(chosen)
}

/// The agent question answered without asking, for `--yes`.
///
/// `--yes` is "press Enter through everything", so it accepts the same default the prompt
/// would show: the effective value where there is one, and otherwise the first agent already
/// authenticated on this host. On an already-configured repo that is a no-op; on a fresh one
/// it is what makes `am setup --yes` produce a config a session can actually start from.
pub fn default_agent_answer(detected: &DetectedState) -> Option<container::KnownAgent> {
    agent_write(detected, detected.default_agent())
}

/// Ask which agent to use, and return the value to write — `None` when nothing changed.
///
/// `agent_flag` is not a prompt default: a flag supplies an answer, so it is evaluated
/// without asking anything, identically with and without `--yes`.
pub fn ask_agent(
    io: &mut dyn Io,
    detected: &DetectedState,
    agent_flag: Option<container::KnownAgent>,
) -> Result<Option<container::KnownAgent>> {
    if let Some(agent) = agent_flag {
        return Ok(agent_write(detected, agent));
    }

    let default = detected.default_agent();
    io.println("Which agent do you use?");
    for (index, agent) in MENU.iter().enumerate() {
        // Presence of credentials, never their contents.
        let note = if detected.has_credentials(*agent) {
            "  (already authenticated on this host)"
        } else {
            ""
        };
        io.println(&format!("  [{}] {agent}{note}", index + 1));
    }
    io.println(&match detected.effective_agent.value {
        Some(agent) => format!(
            "  currently: {agent} ({})",
            detected.effective_agent.source.label()
        ),
        None => "  currently: none configured".to_string(),
    });

    loop {
        let Some(answer) = io.prompt_line(&format!("Agent [1-{}] (Enter for {default}): ", MENU.len()))
        else {
            return Err(eof_aborted());
        };
        if answer.is_empty() {
            return Ok(agent_write(detected, default));
        }
        match parse_agent_answer(&answer) {
            Some(agent) => return Ok(agent_write(detected, agent)),
            None => io.println(&format!(
                "'{answer}' is not one of 1-{} or an agent name.",
                MENU.len()
            )),
        }
    }
}

/// A menu number or an agent name — a user who knows what they want should not have to
/// count list items.
fn parse_agent_answer(answer: &str) -> Option<container::KnownAgent> {
    if let Ok(index) = answer.parse::<usize>() {
        return MENU.get(index.checked_sub(1)?).copied();
    }
    container::KnownAgent::parse(answer).ok()
}

// ── Question 5: containers ────────────────────────────────────────────────────

/// Ask whether to keep running with containers, and return the value to write — `None` when
/// nothing changed.
///
/// Only asked when neither podman nor docker is on PATH: with a runtime present there is
/// nothing ambiguous to resolve. Which runtime to use is never asked — `RuntimePreference::
/// Auto` already answers that.
///
/// Also skipped when there is no global config file to write the answer to (`detected.
/// global_config_path` is `None` — neither `XDG_CONFIG_HOME` nor `HOME` is set): "ask only
/// what detected state can't answer" cuts both ways, and a question whose answer cannot be
/// acted on is worse than not asking it. `cmd_setup` has already told the user about the
/// missing environment once, at the point it skips creating the global file itself.
pub fn ask_container_enabled(io: &mut dyn Io, detected: &DetectedState) -> Result<Option<bool>> {
    if !detected.runtimes_found.is_empty() || detected.global_config_path.is_none() {
        return Ok(None);
    }

    let currently_enabled = detected.effective_container_enabled.value;
    io.println("No container runtime found on this machine (neither podman nor docker).");
    io.println(&format!(
        "  currently: container.enabled = {currently_enabled} ({})",
        detected.effective_container_enabled.source.label()
    ));
    let question = if currently_enabled {
        "Proceed with containers disabled for now? [y/N] "
    } else {
        "Proceed with containers disabled for now? [Y/n] "
    };

    loop {
        let Some(answer) = io.prompt_line(question) else {
            return Err(eof_aborted());
        };
        let disable = if answer.is_empty() {
            !currently_enabled
        } else {
            match parse_yes_no(&answer) {
                Some(yes) => yes,
                None => {
                    io.println(&format!("Answer y or n (got '{answer}')."));
                    continue;
                }
            }
        };
        let enabled = !disable;
        return Ok((enabled != currently_enabled).then_some(enabled));
    }
}

fn parse_yes_no(answer: &str) -> Option<bool> {
    match answer.to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

// ── Step 8: the first session ─────────────────────────────────────────────────

/// Offer to start a first session, returning the slug the user chose.
///
/// `may_build` warns that accepting can take minutes, so a wizard that is building an image
/// does not just appear to hang.
pub fn ask_first_session(io: &mut dyn Io, may_build: bool) -> Result<Option<String>> {
    if may_build {
        io.println("Starting a session may take a few minutes the first time, while the environment is built.");
    }
    let Some(answer) = io.prompt_line("Start your first session now? [Y/n] ") else {
        return Err(eof_aborted());
    };
    if !answer.is_empty() && parse_yes_no(&answer) != Some(true) {
        return Ok(None);
    }

    // A required field with no default. Two tries, then fall through to the next-steps
    // block rather than loop forever on someone who has decided not to answer.
    for _ in 0..2 {
        let Some(slug) = io.prompt_line("Session name: ") else {
            return Err(eof_aborted());
        };
        match crate::cli::validate_slug(&slug) {
            Ok(slug) => return Ok(Some(slug)),
            Err(reason) if slug.is_empty() => io.println(&format!("A name is needed — {reason}.")),
            Err(reason) => io.println(&format!("'{slug}' will not work as a session name — {reason}.")),
        }
    }
    Ok(None)
}

// ── Greenfield file creation ──────────────────────────────────────────────────

/// The project config skeleton, owned by `config` (see `config::render_project_config_
/// skeleton`) since `config::write_defaults` — which predates this module — needs it too.
/// Re-exported here so every existing call site in this file keeps working unqualified.
pub use crate::config::render_project_config_skeleton;

/// The example line inside [`render_project_config_skeleton`] that names the value
/// [`render_project_config_skeleton_with_agent`] activates. Kept as one literal so the two
/// functions cannot describe two different lines; a test pins it against the skeleton itself.
const AGENT_EXAMPLE_LINE: &str =
    "# agent = \"claude\"       # agent to launch, e.g. \"claude\" | \"copilot\" — also selects the container image";

/// [`render_project_config_skeleton`], with `defaults.agent` already active.
///
/// For the one case where `am setup` creates a brand-new project file and already knows the
/// agent to write (a flag, or `--yes`'s default): rendering the line active from the start
/// means it never has to be inserted next to its own commented example, which is what made a
/// freshly created file read like `defaults.agent` was set twice. `update_project_agent` —
/// `toml_edit`, preserving everything else about the file — still owns every other case: a
/// file that already existed, or one created without a known agent yet.
pub fn render_project_config_skeleton_with_agent(agent: container::KnownAgent) -> String {
    let active_line = AGENT_EXAMPLE_LINE
        .trim_start_matches("# ")
        .replacen("\"claude\"", &format!("\"{agent}\""), 1);
    let skeleton = render_project_config_skeleton();
    debug_assert!(
        skeleton.contains(AGENT_EXAMPLE_LINE),
        "AGENT_EXAMPLE_LINE no longer matches render_project_config_skeleton's text"
    );
    skeleton.replacen(AGENT_EXAMPLE_LINE, &active_line, 1)
}

/// The global config skeleton.
///
/// Fully commented out, unlike `am generate-config`'s template: a file `am setup` created
/// on the user's behalf must not silently activate an override they never asked for.
pub fn render_global_config_skeleton() -> &'static str {
    r#"# am global configuration — ~/.config/am/config.toml
# Machine-wide defaults for every project, created by `am setup`.
# Uncomment only the values you want to override from am's compiled-in defaults.
# Precedence (highest wins): CLI flags > environment variables > project config (.am/config.toml) > global config
# Run `am generate-config` to print the full template with every option documented.

[defaults]
# agent = "claude"       # agent to launch: "claude" | "copilot" | "gemini" | "codex"

# Per-agent settings. The compiled-in defaults cover claude and copilot; add an entry for
# any other agent you use.
# [agents.claude]
# image = "ghcr.io/dstanek/am-claude-minimal:latest"

[tmux]
# agent_pane = "left"    # which pane gets the agent: "left" | "right"
# split = "horizontal"   # split direction: "horizontal" | "vertical"
# split_percent = 50     # percentage of the window given to the agent pane (1-99)

[container]
# enabled = true         # false runs sessions directly on the host, with no isolation
# mode = "auto"          # "auto" (devcontainer when one is found) | "devcontainer" | "image"
# runtime = "auto"       # "auto" (podman first, then docker) | "podman" | "docker"
# network = "full"       # "full" | "none"
# env = []               # extra environment variables passed into the container
# gitconfig = ""         # path to gitconfig to mount (default: ~/.gitconfig)
# ssh = ""               # path to SSH dir to mount (default: ~/.ssh)
# image = ""             # override image for all agents (prefer [agents.<name>].image)
# user = "am"            # username inside the container

# Applies only when container.mode resolves to "devcontainer".
[devcontainer]
# path = ""                   # explicit devcontainer.json, relative to the worktree
# cli = "devcontainer"        # CLI binary name or path
# agent_install = "auto"      # "feature" | "bootstrap" | "none" | "auto"
# allow_host_commands = false # let initializeCommand run on YOUR HOST — off by default
# skip_lifecycle = false      # skip postCreateCommand and the other in-container hooks
"#
}

// ── Existing-file updates ─────────────────────────────────────────────────────

/// Set `defaults.agent` in an existing project config.
///
/// Returns `Ok(true)` if the file was written, `Ok(false)` if it already said so — in which
/// case the file is not touched at all, so its mtime does not move.
pub fn update_project_agent(path: &Path, agent: container::KnownAgent) -> Result<bool> {
    update_key(path, "defaults", "agent", Value::from(agent.to_string()))
}

/// Set `container.enabled` in an existing global config. Same `Ok(true)`/`Ok(false)`
/// contract as [`update_project_agent`].
pub fn update_global_container_enabled(path: &Path, enabled: bool) -> Result<bool> {
    update_key(path, "container", "enabled", Value::from(enabled))
}

/// Set one key in an existing TOML file, preserving everything else about it.
///
/// `toml_edit` rather than line matching because the file may have been hand-edited: tables
/// in any order, unrelated keys, comments in the middle of it. Those all have to survive an
/// edit `am` asked to be allowed to make.
fn update_key(path: &Path, table: &str, key: &str, new: Value) -> Result<bool> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing config file {}", path.display()))?;

    let entry = doc.entry(table).or_insert_with(|| Item::Table(Table::new()));
    let table_like = entry.as_table_like_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "'{table}' in {} is not a table — fix it by hand, then re-run 'am setup'",
            path.display()
        )
    })?;

    match table_like.get_mut(key) {
        Some(item) => match item.as_value_mut() {
            // `as_value_mut` returns `Some` for *any* `Value`, not just scalars — an inline
            // table (`agent = { name = "claude" }`) or an array (`agent = ["claude", "x"]`)
            // both are one, and reach here instead of the `None` arm below. Reject them the
            // same way: never overwrite a structural value, only correct a scalar.
            Some(Value::Array(_) | Value::InlineTable(_)) => {
                return Err(structural_value_error(table, key, path));
            }
            Some(existing) => {
                // Compared as parsed values, not as text: a file that already answers the
                // question is left alone entirely.
                if same_value(existing, &new) {
                    return Ok(false);
                }
                // Carry the old decor over so a trailing comment on the line survives.
                let mut new = new;
                *new.decor_mut() = existing.decor().clone();
                *existing = new;
            }
            // Not a `Value` at all — a hand-edited `[defaults.agent]` sub-table or an
            // array-of-tables. `Table::insert` would silently discard it and everything
            // nested under it, so this is an error rather than a clobber too.
            None => {
                return Err(structural_value_error(table, key, path));
            }
        },
        None => {
            table_like.insert(key, Item::Value(new));
        }
    }

    std::fs::write(path, doc.to_string())
        .with_context(|| format!("writing config file {}", path.display()))?;
    Ok(true)
}

/// The key exists but does not hold a plain scalar value — a table, an inline table, an
/// array, or an array-of-tables. `am setup` only ever writes a string or a bool, so any of
/// these shapes means the file was hand-edited into something `am` must not silently discard.
fn structural_value_error(table: &str, key: &str, path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "'{table}.{key}' in {} is not a plain value (found a table or array) — fix it by \
         hand, then re-run 'am setup'",
        path.display()
    )
}

/// Value equality for the two shapes `am setup` writes. A key holding some other type (or
/// a string where a bool belongs) is not equal to anything, so it gets corrected.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use container::KnownAgent;
    use tempfile::TempDir;

    // ── Test doubles ──────────────────────────────────────────────────────────

    /// Replays a fixed list of answers and captures everything printed.
    struct ScriptedIo {
        answers: std::collections::VecDeque<String>,
        output: String,
    }

    impl ScriptedIo {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|a| (*a).to_string()).collect(),
                output: String::new(),
            }
        }
    }

    impl Io for ScriptedIo {
        fn prompt_line(&mut self, question: &str) -> Option<String> {
            self.output.push_str(question);
            let answer = self.answers.pop_front()?;
            self.output.push_str(&answer);
            self.output.push('\n');
            Some(answer.trim().to_string())
        }

        fn println(&mut self, line: &str) {
            self.output.push_str(line);
            self.output.push('\n');
        }
    }

    fn detected(
        agent: Effective<Option<KnownAgent>>,
        enabled: Effective<bool>,
        credentials: &[KnownAgent],
        runtimes: Vec<container::RuntimeKind>,
    ) -> DetectedState {
        DetectedState {
            vcs: Some(config::Vcs::Git),
            project_config_path: PathBuf::from("/repo/.am/config.toml"),
            project_config_exists: true,
            global_config_path: Some(PathBuf::from("/home/u/.config/am/config.toml")),
            global_config_exists: true,
            tmux_present: true,
            runtimes_found: runtimes,
            devcontainer: None,
            agent_credentials: MENU
                .iter()
                .map(|agent| (*agent, credentials.contains(agent)))
                .collect(),
            effective_agent: agent,
            effective_container_enabled: enabled,
        }
    }

    fn configured(agent: Option<KnownAgent>, source: Source) -> DetectedState {
        detected(
            Effective {
                value: agent,
                source,
            },
            Effective {
                value: true,
                source: Source::CompiledDefault,
            },
            &[],
            vec![container::RuntimeKind::Podman],
        )
    }

    // ── The agent question ────────────────────────────────────────────────────

    #[test]
    fn empty_input_accepts_the_effective_value_and_writes_nothing() {
        let state = configured(Some(KnownAgent::Claude), Source::Global);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_agent(&mut io, &state, None).unwrap(), None);
    }

    #[test]
    fn the_prompt_names_the_source_of_the_current_value() {
        let state = configured(Some(KnownAgent::Claude), Source::Global);
        let mut io = ScriptedIo::new(&[""]);

        ask_agent(&mut io, &state, None).unwrap();

        assert!(
            io.output.contains("claude (from your global config)"),
            "prompt did not label the source: {}",
            io.output
        );
    }

    #[test]
    fn nothing_configured_pre_selects_an_authenticated_agent() {
        // The one case where the shown default is not "what is already configured",
        // because nothing is.
        let state = detected(
            Effective {
                value: None,
                source: Source::CompiledDefault,
            },
            Effective {
                value: true,
                source: Source::CompiledDefault,
            },
            &[KnownAgent::Gemini],
            vec![container::RuntimeKind::Podman],
        );
        let mut io = ScriptedIo::new(&[""]);

        // Accepting it is a change, because the effective value was "none".
        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            Some(KnownAgent::Gemini)
        );
        assert!(io.output.contains("Enter for gemini"), "{}", io.output);
    }

    #[test]
    fn nothing_configured_and_no_credentials_falls_back_to_claude() {
        let state = configured(None, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            Some(KnownAgent::Claude)
        );
    }

    #[test]
    fn a_different_answer_is_written_even_when_the_current_value_is_inherited() {
        // The trap: read precedence and write target are different things. The value came
        // from the global config; the change still belongs in the project file.
        let state = configured(Some(KnownAgent::Claude), Source::Global);
        let mut io = ScriptedIo::new(&["4"]);

        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            Some(KnownAgent::Codex)
        );
    }

    #[test]
    fn an_agent_can_be_answered_by_name() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["copilot"]);

        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            Some(KnownAgent::Copilot)
        );
    }

    #[test]
    fn invalid_input_re_asks_with_a_reason() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["9", "nope", "2"]);

        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            Some(KnownAgent::Copilot)
        );
        assert_eq!(io.output.matches("is not one of").count(), 2, "{}", io.output);
    }

    #[test]
    fn zero_is_not_a_menu_item() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["0", "1"]);

        assert_eq!(
            ask_agent(&mut io, &state, None).unwrap(),
            None,
            "1 is claude, which is already the effective value"
        );
    }

    #[test]
    fn end_of_input_aborts_rather_than_looping() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        let err = ask_agent(&mut io, &state, None).unwrap_err();

        assert!(err.to_string().contains("--yes"), "{err}");
    }

    #[test]
    fn yes_accepts_the_same_default_an_enter_press_would() {
        let configured_repo = configured(Some(KnownAgent::Claude), Source::Global);
        assert_eq!(default_agent_answer(&configured_repo), None);

        let fresh_repo = detected(
            Effective {
                value: None,
                source: Source::CompiledDefault,
            },
            Effective {
                value: true,
                source: Source::CompiledDefault,
            },
            &[KnownAgent::Copilot],
            vec![container::RuntimeKind::Podman],
        );
        assert_eq!(
            default_agent_answer(&fresh_repo),
            Some(KnownAgent::Copilot),
            "a fresh repo gets the agent this host is already authenticated for"
        );
    }

    #[test]
    fn the_agent_flag_answers_without_asking() {
        let state = configured(Some(KnownAgent::Codex), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(
            ask_agent(&mut io, &state, Some(KnownAgent::Claude)).unwrap(),
            Some(KnownAgent::Claude)
        );
        assert!(io.output.is_empty(), "flag should not prompt: {}", io.output);
    }

    #[test]
    fn the_agent_flag_matching_the_current_value_writes_nothing() {
        let state = configured(Some(KnownAgent::Codex), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(
            ask_agent(&mut io, &state, Some(KnownAgent::Codex)).unwrap(),
            None
        );
    }

    #[test]
    fn credentials_are_reported_as_presence_only() {
        let state = detected(
            Effective {
                value: None,
                source: Source::CompiledDefault,
            },
            Effective {
                value: true,
                source: Source::CompiledDefault,
            },
            &[KnownAgent::Claude],
            vec![container::RuntimeKind::Podman],
        );
        let mut io = ScriptedIo::new(&[""]);

        ask_agent(&mut io, &state, None).unwrap();

        assert!(io.output.contains("claude  (already authenticated on this host)"));
        assert!(!io.output.contains("copilot  (already"));
    }

    // ── The containers question ───────────────────────────────────────────────

    fn no_runtime(enabled: bool, source: Source) -> DetectedState {
        detected(
            Effective {
                value: None,
                source: Source::CompiledDefault,
            },
            Effective {
                value: enabled,
                source,
            },
            &[],
            Vec::new(),
        )
    }

    #[test]
    fn containers_are_not_asked_about_when_a_runtime_exists() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), None);
        assert!(io.output.is_empty(), "{}", io.output);
    }

    #[test]
    fn containers_are_not_asked_about_without_a_global_config_to_write_to() {
        // Neither XDG_CONFIG_HOME nor HOME was set, so there is nowhere to save an answer —
        // asking anyway would silently drop it. `cmd_setup` already warns about the missing
        // environment once, at the point it skips creating the global file itself.
        let mut state = no_runtime(true, Source::CompiledDefault);
        state.global_config_path = None;
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), None);
        assert!(io.output.is_empty(), "{}", io.output);
    }

    #[test]
    fn declining_to_disable_containers_writes_nothing() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), None);
        assert!(io.output.contains("[y/N]"), "{}", io.output);
    }

    #[test]
    fn agreeing_to_disable_containers_writes_false() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&["y"]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), Some(false));
    }

    #[test]
    fn an_already_disabled_host_defaults_to_staying_disabled() {
        let state = no_runtime(false, Source::Global);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), None);
        assert!(io.output.contains("[Y/n]"), "{}", io.output);
        assert!(io.output.contains("(from your global config)"), "{}", io.output);
    }

    #[test]
    fn re_enabling_containers_writes_true() {
        let state = no_runtime(false, Source::Global);
        let mut io = ScriptedIo::new(&["n"]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), Some(true));
    }

    #[test]
    fn a_junk_answer_re_asks() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&["maybe", "y"]);

        assert_eq!(ask_container_enabled(&mut io, &state).unwrap(), Some(false));
        assert!(io.output.contains("Answer y or n"), "{}", io.output);
    }

    #[test]
    fn end_of_input_aborts_the_containers_question_too() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[]);

        assert!(ask_container_enabled(&mut io, &state).is_err());
    }

    // ── The first-session question ────────────────────────────────────────────

    #[test]
    fn declining_the_first_session_returns_no_slug() {
        let mut io = ScriptedIo::new(&["n"]);
        assert_eq!(ask_first_session(&mut io, false).unwrap(), None);
    }

    #[test]
    fn accepting_the_first_session_returns_the_slug() {
        let mut io = ScriptedIo::new(&["", "my-feature"]);
        assert_eq!(
            ask_first_session(&mut io, false).unwrap(),
            Some("my-feature".to_string())
        );
    }

    #[test]
    fn an_empty_session_name_re_asks_once_then_gives_up() {
        let mut io = ScriptedIo::new(&["y", "", ""]);
        assert_eq!(ask_first_session(&mut io, false).unwrap(), None);
        assert_eq!(io.output.matches("A name is needed").count(), 2);
    }

    #[test]
    fn an_invalid_session_name_is_rejected_with_the_rule_it_broke() {
        let mut io = ScriptedIo::new(&["y", "Not A Slug", "ok-slug"]);
        assert_eq!(
            ask_first_session(&mut io, false).unwrap(),
            Some("ok-slug".to_string())
        );
        assert!(io.output.contains("will not work as a session name"), "{}", io.output);
    }

    #[test]
    fn a_build_is_warned_about_before_it_starts() {
        let mut io = ScriptedIo::new(&["n"]);
        ask_first_session(&mut io, true).unwrap();
        assert!(io.output.contains("few minutes"), "{}", io.output);
    }

    // ── Precedence ────────────────────────────────────────────────────────────

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn the_project_file_wins_and_is_labelled_as_the_source() {
        let tmp = TempDir::new().unwrap();
        let project = write(
            tmp.path(),
            "project.toml",
            "[defaults]\nagent = \"codex\"\n[container]\nenabled = false\n",
        );
        let global = write(
            tmp.path(),
            "global.toml",
            "[defaults]\nagent = \"claude\"\n[container]\nenabled = true\n",
        );

        let (agent, enabled) = resolve_effective(Some(&project), Some(&global));

        assert_eq!(agent.value, Some(KnownAgent::Codex));
        assert_eq!(agent.source, Source::Project);
        assert!(!enabled.value);
        assert_eq!(enabled.source, Source::Project);
    }

    #[test]
    fn the_global_file_is_used_when_the_project_file_is_silent() {
        let tmp = TempDir::new().unwrap();
        let project = write(tmp.path(), "project.toml", "[tmux]\nsplit_percent = 40\n");
        let global = write(
            tmp.path(),
            "global.toml",
            "[defaults]\nagent = \"claude\"\n[container]\nenabled = false\n",
        );

        let (agent, enabled) = resolve_effective(Some(&project), Some(&global));

        assert_eq!(agent.value, Some(KnownAgent::Claude));
        assert_eq!(agent.source, Source::Global);
        assert!(!enabled.value);
        assert_eq!(enabled.source, Source::Global);
    }

    #[test]
    fn compiled_defaults_apply_when_neither_file_sets_anything() {
        let (agent, enabled) = resolve_effective(None, None);

        assert_eq!(agent.value, None);
        assert_eq!(agent.source, Source::CompiledDefault);
        assert!(enabled.value, "containers are on by default");
        assert_eq!(enabled.source, Source::CompiledDefault);
    }

    #[test]
    fn a_skeleton_project_file_sets_nothing() {
        // Everything in it is commented out, so a fresh `am init` repo still inherits.
        let tmp = TempDir::new().unwrap();
        let project = write(
            tmp.path(),
            "config.toml",
            render_project_config_skeleton(),
        );

        let (agent, enabled) = resolve_effective(Some(&project), None);

        assert_eq!(agent.source, Source::CompiledDefault);
        assert_eq!(enabled.source, Source::CompiledDefault);
    }

    #[test]
    fn an_unparseable_file_reads_as_setting_nothing() {
        // doctor reports the parse error a few steps later; setup does not crash first.
        let tmp = TempDir::new().unwrap();
        let project = write(tmp.path(), "config.toml", "this is not = = toml\n");

        let (agent, _) = resolve_effective(Some(&project), None);

        assert_eq!(agent.source, Source::CompiledDefault);
    }

    #[test]
    fn a_malformed_defaults_table_does_not_mask_a_well_formed_container_table() {
        // toml::from_str::<TrackedKeys> would fail the whole read on the bad `defaults.agent`
        // (a number instead of a string), silently losing the good `container.enabled` in
        // the same file. `read_tracked` must treat the two tables as separate failure
        // domains.
        let tmp = TempDir::new().unwrap();
        let project = write(
            tmp.path(),
            "config.toml",
            "[defaults]\nagent = 5\n[container]\nenabled = true\n",
        );

        let (agent, enabled) = resolve_effective(Some(&project), None);

        assert_eq!(
            agent.source,
            Source::CompiledDefault,
            "the malformed field reads as unset, not as an error"
        );
        assert!(enabled.value, "a good neighbor field must survive");
        assert_eq!(enabled.source, Source::Project);
    }

    #[test]
    fn a_malformed_container_table_does_not_mask_a_well_formed_defaults_table() {
        let tmp = TempDir::new().unwrap();
        let project = write(
            tmp.path(),
            "config.toml",
            "[defaults]\nagent = \"claude\"\n[container]\nenabled = \"yes please\"\n",
        );

        let (agent, enabled) = resolve_effective(Some(&project), None);

        assert_eq!(agent.value, Some(KnownAgent::Claude));
        assert_eq!(agent.source, Source::Project);
        assert_eq!(enabled.source, Source::CompiledDefault);
    }

    #[test]
    fn a_project_file_naming_an_unknown_agent_leaves_the_slot_repairable() {
        // The source is the project file, but the value is unusable — so any answer counts
        // as a change and overwrites it.
        let tmp = TempDir::new().unwrap();
        let project = write(tmp.path(), "config.toml", "[defaults]\nagent = \"nope\"\n");

        let (agent, _) = resolve_effective(Some(&project), None);

        assert_eq!(agent.value, None);
        assert_eq!(agent.source, Source::Project);
    }

    // ── Skeletons ─────────────────────────────────────────────────────────────

    /// Every table in the document is empty — nothing is pre-activated.
    fn sets_nothing(text: &str) -> bool {
        fn empty(table: &toml::Table) -> bool {
            table.values().all(|value| match value {
                toml::Value::Table(inner) => empty(inner),
                _ => false,
            })
        }
        let parsed: toml::Table = toml::from_str(text).expect("skeleton is not valid TOML");
        empty(&parsed)
    }

    #[test]
    fn both_skeletons_are_valid_toml_that_activates_nothing() {
        assert!(sets_nothing(render_project_config_skeleton()));
        assert!(sets_nothing(render_global_config_skeleton()));
    }

    #[test]
    fn the_global_skeleton_documents_the_keys_setup_can_write() {
        let text = render_global_config_skeleton();
        assert!(text.contains("# agent = "));
        assert!(text.contains("# enabled = true"));
    }

    #[test]
    fn the_project_skeleton_is_the_file_am_init_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("config.toml");
        config::write_defaults(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            render_project_config_skeleton()
        );
    }

    #[test]
    fn the_agent_aware_skeleton_activates_only_the_agent_line() {
        let text = render_project_config_skeleton_with_agent(KnownAgent::Codex);

        assert!(text.contains("agent = \"codex\""), "{text}");
        // Nothing else in the skeleton was touched — same guarantee `render_project_config_
        // skeleton` gives, minus the one line this function exists to activate.
        let mut without_agent_line = text
            .lines()
            .filter(|line| !line.trim_start().starts_with("agent ="))
            .collect::<Vec<_>>()
            .join("\n");
        without_agent_line.push('\n');
        let mut plain_without_example = render_project_config_skeleton()
            .lines()
            .filter(|line| !line.trim_start().starts_with("# agent ="))
            .collect::<Vec<_>>()
            .join("\n");
        plain_without_example.push('\n');
        assert_eq!(without_agent_line, plain_without_example);
    }

    #[test]
    fn the_agent_aware_skeleton_is_valid_toml_that_sets_only_the_agent() {
        let text = render_project_config_skeleton_with_agent(KnownAgent::Claude);
        let parsed: toml::Table = toml::from_str(&text).expect("skeleton is not valid TOML");
        assert_eq!(
            parsed["defaults"].get("agent").and_then(|v| v.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn the_agent_aware_skeleton_names_each_menu_agent_correctly() {
        for agent in MENU {
            let text = render_project_config_skeleton_with_agent(agent);
            assert!(
                text.contains(&format!("agent = \"{agent}\"")),
                "{agent}: {text}"
            );
        }
    }

    // ── Updating an existing file ─────────────────────────────────────────────

    /// A file someone has actually edited: tables out of the template's order, an unrelated
    /// key, comments above and beside the values.
    const HAND_EDITED: &str = r#"# my project's am config
[container]
enabled = true   # I want isolation here
mode = "image"

[tmux]
split_percent = 70

# which agent this repo uses
[defaults]
agent = "codex"   # switched from claude
custom_key = "left alone"
"#;

    #[test]
    fn updating_the_agent_leaves_the_rest_of_the_file_alone() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", HAND_EDITED);

        assert!(update_project_agent(&path, KnownAgent::Claude).unwrap());

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, HAND_EDITED.replace("\"codex\"", "\"claude\""));
    }

    #[test]
    fn updating_the_agent_keeps_the_comment_on_its_line() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", HAND_EDITED);

        update_project_agent(&path, KnownAgent::Claude).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("agent = \"claude\"   # switched from claude"),
            "{after}"
        );
    }

    #[test]
    fn an_unchanged_agent_does_not_touch_the_file() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", HAND_EDITED);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert!(!update_project_agent(&path, KnownAgent::Codex).unwrap());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_EDITED);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn the_agent_key_is_inserted_into_an_existing_table() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "config.toml",
            "[defaults]\n# agent = \"claude\"\n\n[tmux]\nsplit = \"vertical\"\n",
        );

        assert!(update_project_agent(&path, KnownAgent::Gemini).unwrap());

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent = \"gemini\""), "{after}");
        // The commented example is left where it is — inert documentation next to a live
        // value, which is tidier to leave than to line-match and rewrite.
        assert!(after.contains("# agent = \"claude\""), "{after}");
        assert!(after.contains("split = \"vertical\""), "{after}");
    }

    #[test]
    fn a_missing_table_is_created() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "[tmux]\nsplit_percent = 30\n");

        assert!(update_project_agent(&path, KnownAgent::Claude).unwrap());

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("split_percent = 30"), "{after}");
        let (agent, _) = resolve_effective(Some(&path), None);
        assert_eq!(agent.value, Some(KnownAgent::Claude));
    }

    #[test]
    fn a_dotted_key_is_updated_in_place() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "defaults.agent = \"codex\"\n");

        assert!(update_project_agent(&path, KnownAgent::Claude).unwrap());

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "defaults.agent = \"claude\"\n"
        );
    }

    #[test]
    fn container_enabled_is_written_to_the_global_file() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "config.toml",
            "# global\n[container]\nenabled = true\nnetwork = \"full\"\n",
        );

        assert!(update_global_container_enabled(&path, false).unwrap());

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# global\n[container]\nenabled = false\nnetwork = \"full\"\n"
        );
    }

    #[test]
    fn an_unchanged_container_enabled_does_not_touch_the_file() {
        let tmp = TempDir::new().unwrap();
        let original = "[container]\nenabled = false\n";
        let path = write(tmp.path(), "config.toml", original);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert!(!update_global_container_enabled(&path, false).unwrap());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn container_enabled_is_added_to_a_skeleton_global_file() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        assert!(update_global_container_enabled(&path, false).unwrap());

        let (_, enabled) = resolve_effective(None, Some(&path));
        assert!(!enabled.value);
        assert_eq!(enabled.source, Source::Global);
    }

    #[test]
    fn a_key_of_the_wrong_type_is_corrected() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "[container]\nenabled = \"no\"\n");

        assert!(update_global_container_enabled(&path, false).unwrap());

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[container]\nenabled = false\n"
        );
    }

    #[test]
    fn a_table_slot_holding_something_else_is_an_error_not_a_clobber() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "defaults = 3\n");

        let err = update_project_agent(&path, KnownAgent::Claude).unwrap_err();

        assert!(err.to_string().contains("not a table"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "defaults = 3\n");
    }

    #[test]
    fn a_key_holding_a_sub_table_is_an_error_not_a_clobber() {
        // A hand-edited `[defaults.agent]` (legal TOML, easily confused with
        // `[agents.claude]`) makes `defaults.agent` a table rather than a scalar.
        // `Table::insert` would silently discard it and everything nested under it —
        // that must be an error instead.
        let tmp = TempDir::new().unwrap();
        let content = "[defaults.agent]\nfoo = \"bar\"\n";
        let path = write(tmp.path(), "config.toml", content);

        let err = update_project_agent(&path, KnownAgent::Claude).unwrap_err();

        assert!(err.to_string().contains("defaults.agent"), "{err}");
        assert!(err.to_string().contains("not a plain value"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "the sub-table must survive untouched"
        );
    }

    #[test]
    fn a_key_holding_an_inline_table_is_an_error_not_a_clobber() {
        // `agent = { name = "claude", extra = "keep-me" }` is a `Value::InlineTable`, so it
        // reaches the `Some(existing)` arm rather than the `None` one — a different branch
        // than the `[defaults.agent]` sub-table case, but the same silent-discard risk.
        let tmp = TempDir::new().unwrap();
        let content = "[defaults]\nagent = { name = \"claude\", extra = \"keep-me\" }\n";
        let path = write(tmp.path(), "config.toml", content);

        let err = update_project_agent(&path, KnownAgent::Claude).unwrap_err();

        assert!(err.to_string().contains("defaults.agent"), "{err}");
        assert!(err.to_string().contains("not a plain value"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "the inline table must survive untouched"
        );
    }

    #[test]
    fn a_key_holding_an_array_is_an_error_not_a_clobber() {
        let tmp = TempDir::new().unwrap();
        let content = "[defaults]\nagent = [\"claude\", \"keep-me-too\"]\n";
        let path = write(tmp.path(), "config.toml", content);

        let err = update_project_agent(&path, KnownAgent::Claude).unwrap_err();

        assert!(err.to_string().contains("defaults.agent"), "{err}");
        assert!(err.to_string().contains("not a plain value"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "the array must survive untouched"
        );
    }

    #[test]
    fn container_enabled_holding_an_array_is_an_error_not_a_clobber() {
        // Same `update_key`, same protection, the other call site.
        let tmp = TempDir::new().unwrap();
        let content = "[container]\nenabled = [true, false]\n";
        let path = write(tmp.path(), "config.toml", content);

        let err = update_global_container_enabled(&path, false).unwrap_err();

        assert!(err.to_string().contains("container.enabled"), "{err}");
        assert!(err.to_string().contains("not a plain value"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn a_malformed_file_is_reported_rather_than_overwritten() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "[defaults\nagent =\n");

        assert!(update_project_agent(&path, KnownAgent::Claude).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[defaults\nagent =\n"
        );
    }

    // ── The menu ──────────────────────────────────────────────────────────────

    #[test]
    fn every_menu_entry_round_trips_through_its_own_name() {
        for agent in MENU {
            assert_eq!(KnownAgent::parse(&agent.to_string()).unwrap(), agent);
        }
    }

    #[test]
    fn menu_answers_are_accepted_as_numbers_or_names() {
        assert_eq!(parse_agent_answer("1"), Some(KnownAgent::Claude));
        assert_eq!(parse_agent_answer("4"), Some(KnownAgent::Codex));
        assert_eq!(parse_agent_answer("codex"), Some(KnownAgent::Codex));
        assert_eq!(parse_agent_answer("5"), None);
        assert_eq!(parse_agent_answer("0"), None);
        assert_eq!(parse_agent_answer(""), None);
    }
}
