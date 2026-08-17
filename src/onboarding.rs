//! `am setup` — the guided front door.
//!
//! Everything specific to the question flow lives here: what `am` can work out on its own
//! (`DetectedState`), the questions it cannot ask by detection alone (`ask_agent`,
//! `ask_container_enabled`/`ask_container_consent` — mutually exclusive, see their own doc
//! comments — and `ask_layout`), and the config writes those answers can produce.
//!
//! Two rules shape the module:
//!
//! - **Ask only what detected state cannot answer.** Every prompt shows the *effective*
//!   current value and where it comes from, and accepting it is a guaranteed no-op —
//!   `update_project_agent` / `update_global_container_enabled` / `update_global_tmux_layout`
//!   return `Ok(false)` (or an empty `Vec`) without touching the file at all. That is what
//!   makes a second `am setup` run silent.
//! - **Credentials are probed for presence only.** Nothing here reads, prints, or writes a
//!   secret; `agent_credentials` holds the same booleans `am doctor` already derives.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::color::{self, Color};
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

/// The gap between the longest agent name and its "credentials found" note, so every note
/// lines up in a column regardless of which names have one. The gap itself is a fixed 3
/// spaces; it's the column *width* it's added to that's computed from `MENU` at the call
/// site, so a longer agent name added later doesn't silently misalign the menu.
const MENU_NOTE_GAP: usize = 3;

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

    /// The most specific of several sources, using the same project → global → compiled-
    /// default precedence `resolve_effective` reads with. Used only where several tracked
    /// values are summarized on one line (the layout question's "currently:" line spans all
    /// three `tmux.*` keys at once) — an individual value's own source is always shown
    /// wherever there is room for one.
    fn most_specific(sources: [Source; 3]) -> Source {
        sources
            .into_iter()
            .min_by_key(|source| match source {
                Source::Project => 0,
                Source::Global => 1,
                Source::CompiledDefault => 2,
            })
            .expect("non-empty array")
    }
}

// ── The write-target line, shared by every question ────────────────────────────

/// Where a question's answer is saved — fixed per question, independent of where the
/// *displayed default* happens to be read from (that's [`Source`], a different question:
/// "where did this value come from" vs. "where would a change go").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteScope {
    Project,
    Global,
}

impl WriteScope {
    fn phrase(self) -> &'static str {
        match self {
            WriteScope::Project => "just this repo",
            WriteScope::Global => "every repo on this machine",
        }
    }
}

/// One line, shared by `ask_agent`, `ask_container_enabled`, and `ask_layout` — a single
/// implementation so the wording cannot drift between them, and so it's pinned by one set of
/// tests instead of three copies that could disagree. Printed dim and indented, directly under
/// the question's own header line: the question already says *what* is being asked, so this
/// line only has to answer "where does the answer go" — no label of its own needed.
///
/// `base` is what `path` gets shortened against for display — `detected.repo_root` for
/// `WriteScope::Project`, `detected.home_dir` for `WriteScope::Global` — never the raw
/// absolute path: an un-shortened project path routinely runs to 80+ characters and wraps,
/// defeating the whole point of a line that exists for at-a-glance clarity. `None` (no known
/// base, or `path` isn't actually under it) falls back to the absolute path.
fn write_target_line(scope: WriteScope, path: &Path, base: Option<&Path>) -> String {
    let shown = shorten_for_display(scope, path, base);
    format!("{}; saved to {}.", scope.phrase(), shown.display())
}

/// The one shortening rule both scopes share: relative to `base` when `path` is under it,
/// `~`-prefixed on top of that for `WriteScope::Global` (a repo-relative path already reads
/// clearly without a marker of its own — `.am/config.toml` — while a home-relative one needs
/// the `~` to say so, the same convention a shell prompt uses).
fn shorten_for_display(scope: WriteScope, path: &Path, base: Option<&Path>) -> PathBuf {
    match scope {
        WriteScope::Project => relative_shorten(path, base),
        WriteScope::Global => tilde_shorten(path, base),
    }
}

/// The relative-path half of [`shorten_for_display`]'s `WriteScope::Project` case, exposed on
/// its own for `cmd_setup`'s own status/confirmation lines that print a project path outside a
/// write-target line — it should abbreviate exactly the same way a project write-target does,
/// not grow a second convention. Mirrors [`tilde_shorten`], the same exposure for the global
/// scope.
pub fn relative_shorten(path: &Path, base: Option<&Path>) -> PathBuf {
    base.and_then(|base| path.strip_prefix(base).ok())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// The `~`-substitution half of [`shorten_for_display`]'s `WriteScope::Global` case, exposed
/// on its own for `cmd_setup`'s header line ("Setting up am for the ... repository at ...") —
/// the one place outside a write-target line that shortens a path for display, and it should
/// abbreviate exactly the same way a global write-target does, not grow a second convention.
pub fn tilde_shorten(path: &Path, home: Option<&Path>) -> PathBuf {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(relative) => Path::new("~").join(relative),
        None => path.to_path_buf(),
    }
}

/// Indent and dim one line — the treatment every secondary line in the interactive flow
/// shares: a write-target line, and a "currently: ..." line. Also `pub(crate)` for
/// `main.rs`'s own status lines (the init report under `am setup`, and the "Created ..."
/// line for a freshly written global config) — the same duplication `relative_shorten`/
/// `tilde_shorten` were made `pub` to avoid, now closed for the dim treatment too. `color`
/// is taken as an argument rather than probed here, the same reason `color::paint` itself
/// does: rendering stays pure, so both the colored and the `NO_COLOR` form are directly
/// testable.
pub(crate) fn dim_line(text: &str, color: bool) -> String {
    color::paint(&format!("  {text}"), Color::Dim, color)
}

/// `PaneSide` and `SplitDirection` have no `Display` impl (matching the rest of the codebase,
/// which hand-matches these strings at each call site rather than adding one) — one place for
/// the words that also double as the TOML values `update_global_tmux_layout` writes.
fn pane_side_str(side: &config::PaneSide) -> &'static str {
    match side {
        config::PaneSide::Left => "left",
        config::PaneSide::Right => "right",
    }
}

fn split_str(direction: &config::SplitDirection) -> &'static str {
    match direction {
        config::SplitDirection::Horizontal => "horizontal",
        config::SplitDirection::Vertical => "vertical",
    }
}

/// Names where the agent pane sits, worded the same way the customize sub-flow's own
/// pane-side question (6b) already words it: "left"/"right" for a horizontal split,
/// "top"/"bottom" for a vertical one — a vertical `render_layout` preview can't show
/// proportion, so the words describing *position* shouldn't silently switch back to
/// left/right either.
fn agent_position_str(
    agent_pane: &config::PaneSide,
    split: &config::SplitDirection,
) -> &'static str {
    match (split, agent_pane) {
        (config::SplitDirection::Horizontal, config::PaneSide::Left) => "left",
        (config::SplitDirection::Horizontal, config::PaneSide::Right) => "right",
        (config::SplitDirection::Vertical, config::PaneSide::Left) => "top",
        (config::SplitDirection::Vertical, config::PaneSide::Right) => "bottom",
    }
}

/// One line, printed right alongside a customize preview (both directions, not just the
/// stacked one), stating in text what percentage each pane actually gets. A stacked preview
/// renders as two equally-sized boxes regardless of `percent` — `render_layout`'s own doc
/// comment calls this "cosmetic best-effort" — so without this line a user confirming a
/// lopsided vertical split (e.g. 95/5) would be looking at a picture that contradicts the
/// value about to be saved. Horizontal previews already convey proportion through box width,
/// but this line is shown there too so the wording doesn't quietly differ between the two —
/// the same property in text, every time.
fn layout_proportion_line(
    agent_pane: &config::PaneSide,
    split: &config::SplitDirection,
    percent: u8,
) -> String {
    format!(
        "  agent ({}) gets {percent}%, the other pane gets {}%.",
        agent_position_str(agent_pane, split),
        100 - percent
    )
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
    /// The base `write_target_line` shortens `project_config_path` against for display.
    /// `None` alongside `vcs` being `None`.
    pub repo_root: Option<PathBuf>,
    pub project_config_path: PathBuf,
    pub project_config_exists: bool,
    /// `None` only when neither `XDG_CONFIG_HOME` nor `HOME` is set.
    pub global_config_path: Option<PathBuf>,
    pub global_config_exists: bool,
    /// `$HOME`, kept only so `write_target_line` can show `global_config_path` with `~`
    /// substituted instead of the full absolute path — a question about "every repo on this
    /// machine" earns nothing from also spelling out one specific machine's home directory.
    /// `None` when `HOME` isn't set, in which case the absolute path is shown as-is.
    pub home_dir: Option<PathBuf>,
    pub tmux_present: bool,
    /// Empty, one, or both — the containers question turns on this being empty.
    pub runtimes_found: Vec<container::RuntimeKind>,
    pub devcontainer: Option<PathBuf>,
    /// Presence only. No credential is ever read, displayed, or written.
    pub agent_credentials: Vec<(container::KnownAgent, bool)>,
    pub effective_agent: Effective<Option<container::KnownAgent>>,
    pub effective_container_enabled: Effective<bool>,
    pub effective_tmux_agent_pane: Effective<config::PaneSide>,
    pub effective_tmux_split: Effective<config::SplitDirection>,
    pub effective_tmux_split_percent: Effective<u8>,
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
        let home_dir = std::env::var_os("HOME").map(PathBuf::from);

        let (
            effective_agent,
            effective_container_enabled,
            effective_tmux_agent_pane,
            effective_tmux_split,
            effective_tmux_split_percent,
        ) = resolve_effective(
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
            repo_root: repo_root.map(Path::to_path_buf),
            project_config_path,
            project_config_exists,
            global_config_path,
            global_config_exists,
            home_dir,
            tmux_present: tmux::find_tmux().is_some(),
            runtimes_found,
            devcontainer,
            agent_credentials,
            effective_agent,
            effective_container_enabled,
            effective_tmux_agent_pane,
            effective_tmux_split,
            effective_tmux_split_percent,
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

/// The tracked keys `am setup` reads from one file on its own.
///
/// Deliberately not `config::load_with_global`: that merges the layers into a single answer,
/// and knowing *which* layer an answer came from is the whole point here.
#[derive(Debug, Default)]
struct TrackedKeys {
    defaults: TrackedDefaults,
    container: TrackedContainer,
    tmux: TrackedTmux,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackedDefaults {
    agent: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackedContainer {
    enabled: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TrackedTmux {
    agent_pane: Option<config::PaneSide>,
    split: Option<config::SplitDirection>,
    split_percent: Option<u8>,
}

/// Read the tracked keys out of one config file. A missing or unparseable file reads as
/// "sets nothing" — `doctor::run` is what reports a broken config, and it runs a few steps
/// later against the same file.
///
/// `defaults`, `container`, and `tmux` are deserialized independently rather than as one
/// `toml::from_str::<TrackedKeys>` call: that call fails the whole read the moment any one
/// sub-table is malformed, which would let a bad `defaults.agent` mask a perfectly
/// well-formed `container.enabled` (or `tmux.*`) in the same file, and vice versa — turning
/// one broken key into extra questions asked without cause.
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
    let tmux = root
        .get("tmux")
        .cloned()
        .and_then(|value| value.try_into::<TrackedTmux>().ok())
        .unwrap_or_default();

    TrackedKeys {
        defaults,
        container,
        tmux,
    }
}

/// Resolve every tracked key with project → global → compiled-default precedence, keeping
/// the layer each answer came from.
#[allow(clippy::type_complexity)]
fn resolve_effective(
    project: Option<&Path>,
    global: Option<&Path>,
) -> (
    Effective<Option<container::KnownAgent>>,
    Effective<bool>,
    Effective<config::PaneSide>,
    Effective<config::SplitDirection>,
    Effective<u8>,
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

    let default_tmux = config::TmuxConfig::default();
    let agent_pane = match (project.tmux.agent_pane, global.tmux.agent_pane) {
        (Some(value), _) => Effective {
            value,
            source: Source::Project,
        },
        (None, Some(value)) => Effective {
            value,
            source: Source::Global,
        },
        (None, None) => Effective {
            value: default_tmux.agent_pane,
            source: Source::CompiledDefault,
        },
    };
    let split = match (project.tmux.split, global.tmux.split) {
        (Some(value), _) => Effective {
            value,
            source: Source::Project,
        },
        (None, Some(value)) => Effective {
            value,
            source: Source::Global,
        },
        (None, None) => Effective {
            value: default_tmux.split,
            source: Source::CompiledDefault,
        },
    };
    let split_percent = match (project.tmux.split_percent, global.tmux.split_percent) {
        (Some(value), _) => Effective {
            value,
            source: Source::Project,
        },
        (None, Some(value)) => Effective {
            value,
            source: Source::Global,
        },
        (None, None) => Effective {
            value: default_tmux.split_percent,
            source: Source::CompiledDefault,
        },
    };

    (agent, enabled, agent_pane, split, split_percent)
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
///
/// `color` governs only the dim secondary lines (the write-target line, the "currently: ..."
/// line) — never the menu itself, which is the actual content being chosen from.
pub fn ask_agent(
    io: &mut dyn Io,
    detected: &DetectedState,
    agent_flag: Option<container::KnownAgent>,
    color: bool,
) -> Result<Option<container::KnownAgent>> {
    if let Some(agent) = agent_flag {
        return Ok(agent_write(detected, agent));
    }

    let default = detected.default_agent();
    // Leading blank line: the phase separator between whatever came before (the init report,
    // or a previous question) and this one. Printed only once IO actually starts, so a flag or
    // `--yes` answering this question above without a prompt never leaves a stray blank line.
    io.println("");
    io.println("Which agent do you use?");
    io.println(&dim_line(
        &write_target_line(
            WriteScope::Project,
            &detected.project_config_path,
            detected.repo_root.as_deref(),
        ),
        color,
    ));
    io.println("");
    let width = MENU.iter().map(|a| a.to_string().len()).max().unwrap_or(0) + MENU_NOTE_GAP;
    for (index, agent) in MENU.iter().enumerate() {
        // Presence of credentials, never their contents.
        if detected.has_credentials(*agent) {
            // `KnownAgent`'s `Display` impl writes its name with a plain `write!`, which does
            // not honor a width/alignment request from the caller (that requires `f.pad`,
            // which it doesn't use) — going through an owned `String` first sidesteps that,
            // since `str`'s own `Display` impl does.
            let name = agent.to_string();
            io.println(&format!("  [{}] {name:<width$}credentials found", index + 1));
        } else {
            io.println(&format!("  [{}] {agent}", index + 1));
        }
    }
    io.println("");
    io.println(&dim_line(
        &match detected.effective_agent.value {
            Some(agent) => format!("currently: {agent} ({})", detected.effective_agent.source.label()),
            None => "currently: none configured".to_string(),
        },
        color,
    ));
    // Nothing configured anywhere, and no agent has credentials found for it either — the
    // genuine "am has no basis for a guess" case. Without this line, a user would have to
    // infer the fallback from "currently: none configured" plus "Enter for claude" below;
    // stating it explicitly removes that inference.
    if detected.effective_agent.value.is_none()
        && !detected.agent_credentials.iter().any(|(_, present)| *present)
    {
        io.println(&dim_line(
            "nothing found configured or credentialed on this host — defaulting to claude.",
            color,
        ));
    }
    io.println("");

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
pub fn ask_container_enabled(io: &mut dyn Io, detected: &DetectedState, color: bool) -> Result<Option<bool>> {
    if !detected.runtimes_found.is_empty() {
        return Ok(None);
    }
    let Some(path) = detected.global_config_path.as_deref() else {
        return Ok(None);
    };

    let currently_enabled = detected.effective_container_enabled.value;
    io.println("");
    io.println("No container runtime found on this machine (neither podman nor docker).");
    io.println(&dim_line(
        &write_target_line(WriteScope::Global, path, detected.home_dir.as_deref()),
        color,
    ));
    io.println("");
    io.println(&dim_line(
        &format!(
            "currently: container.enabled = {currently_enabled} ({})",
            detected.effective_container_enabled.source.label()
        ),
        color,
    ));
    io.println("");
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

/// Ask, on a fresh setup, whether the user wants sessions containerised at all — the
/// informed-consent framing. Unlike [`ask_container_enabled`], this is not gated on whether
/// a runtime is currently found: that's the wrong question for a user who may not know
/// containers are involved at all. Only gated on there being a global file to write the
/// answer to (`detected.global_config_path` is `Some`) — same "nowhere to save it" rule
/// every other question in this module uses.
///
/// `cmd_setup` calls this only when `!detected.global_config_exists` and calls
/// `ask_container_enabled` otherwise — never both in the same run.
pub fn ask_container_consent(
    io: &mut dyn Io,
    detected: &DetectedState,
    color: bool,
) -> Result<Option<bool>> {
    let Some(path) = detected.global_config_path.as_deref() else {
        return Ok(None);
    };

    let currently_enabled = detected.effective_container_enabled.value;
    io.println("");
    io.println("Use isolated containers for your sessions?");
    io.println(&dim_line(
        &write_target_line(WriteScope::Global, path, detected.home_dir.as_deref()),
        color,
    ));
    io.println("");
    io.println(
        "Each session gets its own isolated filesystem and process sandbox. Without \
         containers, sessions run directly on the host.",
    );
    if detected.runtimes_found.is_empty() {
        io.println(&dim_line(
            "no container runtime was found on this machine yet — you can still opt in \
             and install one before starting a session.",
            color,
        ));
    }
    io.println("");

    loop {
        let Some(answer) = io.prompt_line("Use isolated containers for your sessions? [Y/n] ")
        else {
            return Err(eof_aborted());
        };
        let enabled = if answer.is_empty() {
            true
        } else {
            match parse_yes_no(&answer) {
                Some(yes) => yes,
                None => {
                    io.println(&format!("Answer y or n (got '{answer}')."));
                    continue;
                }
            }
        };
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

// ── Question 6: pane layout ───────────────────────────────────────────────────

/// The four fixed presets `ask_layout`'s menu offers, in the order shown. Preset 1 is `am`'s
/// compiled default (`TmuxConfig::default()`), which is why it's what a first-time user with
/// nothing configured sees pre-selected.
const LAYOUT_PRESETS: [(config::PaneSide, config::SplitDirection, u8); 4] = [
    (config::PaneSide::Left, config::SplitDirection::Horizontal, 50),
    (config::PaneSide::Right, config::SplitDirection::Horizontal, 50),
    (config::PaneSide::Left, config::SplitDirection::Horizontal, 70),
    (config::PaneSide::Left, config::SplitDirection::Vertical, 50),
];

/// One label per entry of [`LAYOUT_PRESETS`], same order — kept as a parallel array rather
/// than derived from the triple because "stacked, agent on top" is a name for the *shape*,
/// not something worth re-deriving from `(PaneSide, SplitDirection, u8)` for a fixed set of
/// four.
const LAYOUT_PRESET_LABELS: [&str; 4] = [
    "agent left, 50/50",
    "agent right, 50/50",
    "agent left, 70/30",
    "stacked, agent on top, 50/50",
];

/// Printed once, before the layout question's menu, only when the project config already
/// pins its own value for at least one of the three keys this question writes. A project-
/// level `tmux.*` override outranks the global file for this repo specifically, so an answer
/// here would otherwise look like it silently did nothing the next time a session starts in
/// this repo.
const PROJECT_LAYOUT_OVERRIDE_NOTE: &str = "this project's config already sets its own \
    pane layout — your answer here is saved globally and won't change sessions in this repo \
    until that override is removed.";

fn layout_has_project_override(detected: &DetectedState) -> bool {
    detected.effective_tmux_agent_pane.source == Source::Project
        || detected.effective_tmux_split.source == Source::Project
        || detected.effective_tmux_split_percent.source == Source::Project
}

/// The menu's "currently: ..." line, spanning all three keys at once. Its single source
/// label is the most specific of the three (see [`Source::most_specific`]) — in the common
/// case all three come from the same file, and this reads correctly there; a config that
/// mixes sources across the three keys is an edge case this line does not attempt to spell
/// out further, since the per-key sub-questions inside `ask_layout_custom` each show their
/// own value's actual source.
fn current_layout_line(detected: &DetectedState) -> String {
    let source = Source::most_specific([
        detected.effective_tmux_agent_pane.source,
        detected.effective_tmux_split.source,
        detected.effective_tmux_split_percent.source,
    ]);
    format!(
        "currently: agent_pane={}, split={}, split_percent={} ({})",
        pane_side_str(&detected.effective_tmux_agent_pane.value),
        split_str(&detected.effective_tmux_split.value),
        detected.effective_tmux_split_percent.value,
        source.label()
    )
}

/// Interior width of the two-pane preview box, in characters — wide enough that a 50/50 (or
/// 70/30) split reads comfortably, narrow enough to sit next to a preset's label on one line.
const PREVIEW_WIDTH: usize = 20;

/// A small fixed-width ASCII diagram, [`PREVIEW_WIDTH`] characters wide, proportioned by
/// `percent` and clamped so neither label is ever squeezed below its own length. Used both
/// for the four preset previews and for the one customize preview, so there is exactly one
/// rendering implementation to keep correct.
///
/// A horizontal split renders as one line, two boxes side by side, proportioned by width. A
/// vertical (stacked) split renders as two lines, one box per line — height can't be
/// meaningfully proportioned in single-line ASCII the way width can, so a stacked preview
/// always shows two equally-sized boxes regardless of `percent`. `percent` still governs
/// which value actually gets written; it just doesn't change how the stacked preview looks,
/// which is exactly what makes this a "cosmetic best-effort" render — see the spec.
fn render_layout(
    agent_pane: &config::PaneSide,
    split: &config::SplitDirection,
    percent: u8,
) -> Vec<String> {
    const AGENT: &str = "agent";
    const OTHER: &str = "shell";

    match split {
        config::SplitDirection::Horizontal => {
            let (left_label, right_label, left_percent) = match agent_pane {
                config::PaneSide::Left => (AGENT, OTHER, percent),
                config::PaneSide::Right => (OTHER, AGENT, 100 - percent),
            };
            vec![render_horizontal_box(left_label, right_label, left_percent)]
        }
        config::SplitDirection::Vertical => {
            let (top_label, bottom_label) = match agent_pane {
                config::PaneSide::Left => (AGENT, OTHER),
                config::PaneSide::Right => (OTHER, AGENT),
            };
            vec![render_stacked_box(top_label), render_stacked_box(bottom_label)]
        }
    }
}

fn render_horizontal_box(left_label: &str, right_label: &str, left_percent: u8) -> String {
    let left_min = left_label.len() + 2;
    let right_min = right_label.len() + 2;
    let total = PREVIEW_WIDTH.max(left_min + right_min);
    let raw_left = total * left_percent as usize / 100;
    let left_width = raw_left.clamp(left_min, total - right_min);
    let right_width = total - left_width;
    format!("[{left_label:^left_width$}|{right_label:^right_width$}]")
}

fn render_stacked_box(label: &str) -> String {
    let width = PREVIEW_WIDTH.max(label.len() + 2);
    format!("[{label:^width$}]")
}

/// The write implied by choosing `(agent_pane, split, split_percent)`, or `None` when it
/// exactly matches what all three keys already resolve to.
fn layout_write(
    detected: &DetectedState,
    agent_pane: config::PaneSide,
    split: config::SplitDirection,
    split_percent: u8,
) -> Option<(config::PaneSide, config::SplitDirection, u8)> {
    let unchanged = detected.effective_tmux_agent_pane.value == agent_pane
        && detected.effective_tmux_split.value == split
        && detected.effective_tmux_split_percent.value == split_percent;
    (!unchanged).then_some((agent_pane, split, split_percent))
}

/// Ask for a pane layout — the preset menu, falling through to [`ask_layout_custom`] on
/// "customize…" — and return the chosen `(agent_pane, split, split_percent)` triple to write,
/// or `None` when it exactly matches what's already effective.
///
/// Not asked at all when `detected.global_config_path` is `None` (same rule as
/// `ask_container_enabled`); `cmd_setup` is what skips calling this under `--yes`.
///
/// The outer `loop` is what re-shows the whole menu (write-target line, caveat, presets, and
/// all) when the customize sub-flow's preview is declined — a plain iteration rather than
/// `ask_layout_custom` recursing back into this function, so repeated declines cannot grow the
/// call stack.
pub fn ask_layout(
    io: &mut dyn Io,
    detected: &DetectedState,
    color: bool,
) -> Result<Option<(config::PaneSide, config::SplitDirection, u8)>> {
    let Some(path) = detected.global_config_path.as_deref() else {
        return Ok(None);
    };
    // What Enter resolves to — the currently effective triple, not a hardcoded preset. This
    // mirrors `ask_agent`'s `default` (effective value) and `ask_container_enabled`'s
    // `currently_enabled` (also effective value): every question in this flow accepts Enter
    // as "keep what's already in effect," never a fixed preset unrelated to detected state.
    let default_triple = (
        detected.effective_tmux_agent_pane.value.clone(),
        detected.effective_tmux_split.value.clone(),
        detected.effective_tmux_split_percent.value,
    );

    loop {
        // Leading blank line: same phase-separator role as `ask_agent`'s and
        // `ask_container_enabled`'s own leading blank — re-printed here too, since declining
        // the customize preview loops back to the top of this same block.
        io.println("");
        io.println("Which layout do you want?");
        io.println(&dim_line(
            &write_target_line(WriteScope::Global, path, detected.home_dir.as_deref()),
            color,
        ));
        if layout_has_project_override(detected) {
            io.println(&format!(
                "{} {PROJECT_LAYOUT_OVERRIDE_NOTE}",
                color::note_prefix(color)
            ));
        }
        io.println("");
        for (index, (side, split, percent)) in LAYOUT_PRESETS.iter().enumerate() {
            io.println(&format!("  [{}] {}", index + 1, LAYOUT_PRESET_LABELS[index]));
            for line in render_layout(side, split, *percent) {
                io.println(&format!("      {line}"));
            }
        }
        io.println("  [5] customize…");
        io.println("");
        io.println(&dim_line(&current_layout_line(detected), color));
        io.println("");

        let preset = loop {
            let Some(answer) = io.prompt_line("Layout [1-5] (Enter to keep current): ") else {
                return Err(eof_aborted());
            };
            if answer.is_empty() {
                break Some(default_triple.clone());
            } else if answer.eq_ignore_ascii_case("customize") {
                break None;
            } else {
                match answer.parse::<usize>() {
                    Ok(n @ 1..=4) => break Some(LAYOUT_PRESETS[n - 1].clone()),
                    Ok(5) => break None,
                    _ => io.println(&format!("'{answer}' is not one of 1-5 or 'customize'.")),
                }
            }
        };

        let (agent_pane, split, split_percent) = match preset {
            Some(triple) => triple,
            // `None` here means the customize preview was declined, not EOF (which returns
            // early above) — back to the top of this loop, which re-shows the menu.
            None => match ask_layout_custom(io, detected, color)? {
                Some(triple) => triple,
                None => continue,
            },
        };
        return Ok(layout_write(detected, agent_pane, split, split_percent));
    }
}

/// The customize sub-flow: direction, then a direction-worded pane-side question, then
/// percent, then a preview-and-confirm.
///
/// Returns `Ok(Some(triple))` on confirmation, `Ok(None)` when the preview is declined — the
/// caller (`ask_layout`'s own loop) is what re-shows the preset menu in that case, not this
/// function recursing into it.
fn ask_layout_custom(
    io: &mut dyn Io,
    detected: &DetectedState,
    color: bool,
) -> Result<Option<(config::PaneSide, config::SplitDirection, u8)>> {
    // 6a: direction first — the pane-side question's wording (6b) depends on it, so it can't
    // be asked second. Each sub-question gets its own leading blank line, the same
    // phase-separator rhythm the three top-level questions use.
    let default_split = &detected.effective_tmux_split.value;
    io.println("");
    io.println("Side by side, or stacked?");
    io.println("");
    io.println("  [1] side by side (horizontal)");
    io.println("  [2] stacked (vertical)");
    io.println("");
    io.println(&dim_line(
        &format!(
            "currently: split={} ({})",
            split_str(default_split),
            detected.effective_tmux_split.source.label()
        ),
        color,
    ));
    io.println("");
    let default_split_choice: u8 = match default_split {
        config::SplitDirection::Horizontal => 1,
        config::SplitDirection::Vertical => 2,
    };
    let split = loop {
        let Some(answer) =
            io.prompt_line(&format!("Direction [1-2] (Enter for {default_split_choice}): "))
        else {
            return Err(eof_aborted());
        };
        let choice = if answer.is_empty() {
            Some(default_split_choice)
        } else {
            answer.parse::<u8>().ok()
        };
        match choice {
            Some(1) => break config::SplitDirection::Horizontal,
            Some(2) => break config::SplitDirection::Vertical,
            _ => io.println(&format!("'{answer}' is not 1 or 2.")),
        }
    };

    // 6b: the pane-side question, worded to match the direction just chosen — "left"/"right"
    // for a horizontal split, "top"/"bottom" for a vertical one. `PaneSide::Left` is the
    // value stored either way; only the words in the prompt change.
    let (question, option_1, option_2) = match split {
        config::SplitDirection::Horizontal => {
            ("Which side should the agent be on?", "left", "right")
        }
        config::SplitDirection::Vertical => (
            "Should the agent be on top or on the bottom?",
            "top",
            "bottom",
        ),
    };
    let default_side = &detected.effective_tmux_agent_pane.value;
    io.println("");
    io.println(question);
    io.println("");
    io.println(&format!("  [1] {option_1}"));
    io.println(&format!("  [2] {option_2}"));
    io.println("");
    io.println(&dim_line(
        &format!(
            "currently: agent_pane={} ({})",
            pane_side_str(default_side),
            detected.effective_tmux_agent_pane.source.label()
        ),
        color,
    ));
    io.println("");
    let default_side_choice: u8 = match default_side {
        config::PaneSide::Left => 1,
        config::PaneSide::Right => 2,
    };
    let agent_pane = loop {
        let Some(answer) = io.prompt_line(&format!("[1-2] (Enter for {default_side_choice}): "))
        else {
            return Err(eof_aborted());
        };
        let choice = if answer.is_empty() {
            Some(default_side_choice)
        } else {
            answer.parse::<u8>().ok()
        };
        match choice {
            Some(1) => break config::PaneSide::Left,
            Some(2) => break config::PaneSide::Right,
            _ => io.println(&format!("'{answer}' is not 1 or 2.")),
        }
    };

    // 6c: percent — reuses `config::validate_split_percent`'s 1-99 constraint rather than
    // reimplementing it.
    let default_percent = detected.effective_tmux_split_percent.value;
    io.println("");
    io.println("What percentage of the window should the agent pane get?");
    io.println("");
    io.println(&dim_line(
        &format!(
            "currently: split_percent={default_percent} ({})",
            detected.effective_tmux_split_percent.source.label()
        ),
        color,
    ));
    io.println("");
    let split_percent = loop {
        let Some(answer) = io.prompt_line(&format!("[1-99] (Enter for {default_percent}): "))
        else {
            return Err(eof_aborted());
        };
        if answer.is_empty() {
            break default_percent;
        }
        match answer.parse::<u8>() {
            Ok(n) if config::validate_split_percent(n).is_ok() => break n,
            _ => io.println(&format!("'{answer}' must be a number between 1 and 99.")),
        }
    };

    // 6d: preview and confirm. The preview and the proportion line stay tight against the
    // statement introducing them — they're a figure and its caption, not separate phases —
    // with one blank line before the final yes/no prompt.
    io.println("");
    io.println("Here's what that looks like:");
    for line in render_layout(&agent_pane, &split, split_percent) {
        io.println(&format!("  {line}"));
    }
    // The preview picture alone can't be trusted for proportion on a stacked (vertical)
    // split — see `layout_proportion_line` — so the confirm prompt is never based on the
    // picture alone.
    io.println(&layout_proportion_line(&agent_pane, &split, split_percent));
    io.println("");
    let Some(answer) = io.prompt_line("Use this layout? [Y/n] ") else {
        return Err(eof_aborted());
    };
    if answer.is_empty() || parse_yes_no(&answer) == Some(true) {
        return Ok(Some((agent_pane, split, split_percent)));
    }

    // Declined: the caller's loop re-shows the preset menu, not a partial retry of 6a-6c.
    Ok(None)
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
# allow_runtime_escalation = false # let privileged, capAdd, securityOpt, and runArgs apply
# allow_host_mounts = false   # honour a bind mount whose source is outside the worktree
# skip_lifecycle = false      # skip postCreateCommand and the other in-container hooks
"#
}

// ── Skeleton cleanup ──────────────────────────────────────────────────────────
//
// A project or global config file that starts out as our own fully-commented skeleton —
// whether written moments ago by this run, by an earlier `am init`/`am setup` invocation, by
// an older `am`, or checked into the repo by a teammate — has a commented example line for
// every key `am setup` knows how to write. When an interactive answer activates one of those
// keys (the value wasn't known yet at skeleton-creation time the way `--agent`/`--yes` are —
// see `render_project_config_skeleton_with_agent`), `update_key` inserts the real key as new
// text, leaving the original commented example sitting right above it — reading as if the
// same setting were made twice. The four functions below remove exactly that stale example
// line.
//
// This is *not* gated on when or by whom the file was created: `strip_skeleton_example` only
// ever removes a line that is byte-for-byte identical to our own skeleton's — never a line
// that merely looks similar (different spacing, a different trailing comment, one a user
// deliberately kept). That exact-match requirement is the entire safety story now that
// there's no "only touch a file this run just wrote" gate to fall back on; see the
// near-miss test below for the boundary it draws.

/// The commented example lines inside [`render_global_config_skeleton`] that each of the
/// three `tmux.*` keys and `container.enabled` activate. Kept as literals — like
/// [`AGENT_EXAMPLE_LINE`] — so the skeleton's text and the cleanup logic cannot silently
/// drift apart; a test pins each one against the skeleton itself.
const TMUX_AGENT_PANE_EXAMPLE_LINE: &str =
    "# agent_pane = \"left\"    # which pane gets the agent: \"left\" | \"right\"";
const TMUX_SPLIT_EXAMPLE_LINE: &str =
    "# split = \"horizontal\"   # split direction: \"horizontal\" | \"vertical\"";
const TMUX_SPLIT_PERCENT_EXAMPLE_LINE: &str =
    "# split_percent = 50     # percentage of the window given to the agent pane (1-99)";
const CONTAINER_ENABLED_EXAMPLE_LINE: &str =
    "# enabled = true         # false runs sessions directly on the host, with no isolation";

/// Remove one commented example line from a config file, once an interactive answer has
/// activated the key that line documents — regardless of when or by whom the file was
/// written.
///
/// A no-op, not an error, when `example_line` is not found verbatim. That covers two distinct
/// cases the caller does not need to tell apart: the flag/`--yes` path pre-activated the line
/// at creation instead (see [`render_project_config_skeleton_with_agent`]), so there was never
/// a stale comment to remove; or the file's line merely resembles the example — different
/// spacing, a different trailing comment, a line a user deliberately kept — in which case it
/// is not our text and must survive untouched. Byte-exact identity to `example_line` is the
/// only signal this function trusts.
fn strip_skeleton_example(path: &Path, example_line: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    if !text.lines().any(|line| line == example_line) {
        return Ok(());
    }

    let mut new_text = String::with_capacity(text.len());
    for line in text.lines() {
        if line == example_line {
            continue;
        }
        new_text.push_str(line);
        new_text.push('\n');
    }
    std::fs::write(path, new_text)
        .with_context(|| format!("writing config file {}", path.display()))?;
    Ok(())
}

/// Clean up after [`update_project_agent`].
pub fn strip_project_agent_example(path: &Path) -> Result<()> {
    strip_skeleton_example(path, AGENT_EXAMPLE_LINE)
}

/// Clean up after [`update_global_container_enabled`].
pub fn strip_global_container_enabled_example(path: &Path) -> Result<()> {
    strip_skeleton_example(path, CONTAINER_ENABLED_EXAMPLE_LINE)
}

/// Clean up after [`update_global_tmux_layout`] — `written_keys` is that call's own return
/// value, so only the keys actually written have their example line removed; a customize
/// answer that only changed `split_percent` leaves the still-accurate `agent_pane`/`split`
/// examples in place.
pub fn strip_global_tmux_layout_examples(path: &Path, written_keys: &[&str]) -> Result<()> {
    for key in written_keys {
        let example_line = match *key {
            "agent_pane" => TMUX_AGENT_PANE_EXAMPLE_LINE,
            "split" => TMUX_SPLIT_EXAMPLE_LINE,
            "split_percent" => TMUX_SPLIT_PERCENT_EXAMPLE_LINE,
            _ => continue,
        };
        strip_skeleton_example(path, example_line)?;
    }
    Ok(())
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

/// Set all three `tmux.*` layout keys in an existing global config, one [`update_key`] call
/// per key — so a customize answer that only changes `split_percent` doesn't also touch
/// already-correct `agent_pane`/`split` lines (and their comments). Returns the names of the
/// keys that were actually written, a subset of `["agent_pane", "split", "split_percent"]`,
/// empty when the chosen layout already matched what was there.
pub fn update_global_tmux_layout(
    path: &Path,
    agent_pane: config::PaneSide,
    split: config::SplitDirection,
    split_percent: u8,
) -> Result<Vec<&'static str>> {
    let mut written = Vec::new();
    if update_key(
        path,
        "tmux",
        "agent_pane",
        Value::from(pane_side_str(&agent_pane).to_string()),
    )? {
        written.push("agent_pane");
    }
    if update_key(
        path,
        "tmux",
        "split",
        Value::from(split_str(&split).to_string()),
    )? {
        written.push("split");
    }
    if update_key(
        path,
        "tmux",
        "split_percent",
        Value::from(i64::from(split_percent)),
    )? {
        written.push("split_percent");
    }
    Ok(written)
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

/// Value equality for the three shapes `am setup` writes (a string, a bool, or — since the
/// layout revision — an integer for `split_percent`). A key holding some other type (or a
/// string where a bool belongs) is not equal to anything, so it gets corrected.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
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
            repo_root: Some(PathBuf::from("/repo")),
            project_config_path: PathBuf::from("/repo/.am/config.toml"),
            project_config_exists: true,
            global_config_path: Some(PathBuf::from("/home/u/.config/am/config.toml")),
            global_config_exists: true,
            home_dir: Some(PathBuf::from("/home/u")),
            tmux_present: true,
            runtimes_found: runtimes,
            devcontainer: None,
            agent_credentials: MENU
                .iter()
                .map(|agent| (*agent, credentials.contains(agent)))
                .collect(),
            effective_agent: agent,
            effective_container_enabled: enabled,
            effective_tmux_agent_pane: Effective {
                value: config::PaneSide::Left,
                source: Source::CompiledDefault,
            },
            effective_tmux_split: Effective {
                value: config::SplitDirection::Horizontal,
                source: Source::CompiledDefault,
            },
            effective_tmux_split_percent: Effective {
                value: 50,
                source: Source::CompiledDefault,
            },
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

        assert_eq!(ask_agent(&mut io, &state, None, false).unwrap(), None);
    }

    #[test]
    fn the_prompt_names_the_source_of_the_current_value() {
        let state = configured(Some(KnownAgent::Claude), Source::Global);
        let mut io = ScriptedIo::new(&[""]);

        ask_agent(&mut io, &state, None, false).unwrap();

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
            ask_agent(&mut io, &state, None, false).unwrap(),
            Some(KnownAgent::Gemini)
        );
        assert!(io.output.contains("Enter for gemini"), "{}", io.output);
        // Gemini's credentials were found, so the "nothing found anywhere" fallback note —
        // which only applies when *no* agent has credentials — must not appear here.
        assert!(
            !io.output.contains("defaulting to claude"),
            "{}",
            io.output
        );
    }

    #[test]
    fn nothing_configured_and_no_credentials_falls_back_to_claude() {
        let state = configured(None, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(
            ask_agent(&mut io, &state, None, false).unwrap(),
            Some(KnownAgent::Claude)
        );
        assert!(
            io.output.contains("nothing found configured or credentialed on this host"),
            "{}",
            io.output
        );
    }

    #[test]
    fn an_already_configured_agent_suppresses_the_fallback_note_even_with_no_credentials() {
        // Gated on effective_agent.value being None, not on credentials alone — a repo that
        // already names an agent is not the "am has no basis for a guess" case, even if that
        // agent's own credentials happen to be missing.
        let state = configured(Some(KnownAgent::Codex), Source::Project);
        let mut io = ScriptedIo::new(&[""]);

        ask_agent(&mut io, &state, None, false).unwrap();

        assert!(
            !io.output.contains("defaulting to claude"),
            "{}",
            io.output
        );
    }

    #[test]
    fn a_different_answer_is_written_even_when_the_current_value_is_inherited() {
        // The trap: read precedence and write target are different things. The value came
        // from the global config; the change still belongs in the project file.
        let state = configured(Some(KnownAgent::Claude), Source::Global);
        let mut io = ScriptedIo::new(&["4"]);

        assert_eq!(
            ask_agent(&mut io, &state, None, false).unwrap(),
            Some(KnownAgent::Codex)
        );
    }

    #[test]
    fn an_agent_can_be_answered_by_name() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["copilot"]);

        assert_eq!(
            ask_agent(&mut io, &state, None, false).unwrap(),
            Some(KnownAgent::Copilot)
        );
    }

    #[test]
    fn invalid_input_re_asks_with_a_reason() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["9", "nope", "2"]);

        assert_eq!(
            ask_agent(&mut io, &state, None, false).unwrap(),
            Some(KnownAgent::Copilot)
        );
        assert_eq!(io.output.matches("is not one of").count(), 2, "{}", io.output);
    }

    #[test]
    fn zero_is_not_a_menu_item() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["0", "1"]);

        assert_eq!(
            ask_agent(&mut io, &state, None, false).unwrap(),
            None,
            "1 is claude, which is already the effective value"
        );
    }

    #[test]
    fn end_of_input_aborts_rather_than_looping() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        let err = ask_agent(&mut io, &state, None, false).unwrap_err();

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
            ask_agent(&mut io, &state, Some(KnownAgent::Claude), false).unwrap(),
            Some(KnownAgent::Claude)
        );
        assert!(io.output.is_empty(), "flag should not prompt: {}", io.output);
    }

    #[test]
    fn the_agent_flag_matching_the_current_value_writes_nothing() {
        let state = configured(Some(KnownAgent::Codex), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(
            ask_agent(&mut io, &state, Some(KnownAgent::Codex), false).unwrap(),
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

        ask_agent(&mut io, &state, None, false).unwrap();

        assert!(io.output.contains("claude    credentials found"), "{}", io.output);
        assert!(!io.output.contains("copilot   credentials found"), "{}", io.output);
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

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), None);
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

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), None);
        assert!(io.output.is_empty(), "{}", io.output);
    }

    #[test]
    fn declining_to_disable_containers_writes_nothing() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), None);
        assert!(io.output.contains("[y/N]"), "{}", io.output);
    }

    #[test]
    fn ask_container_enabled_states_where_the_answer_will_be_saved() {
        // Since `write_target_line` dropped its question-name label, the containers and layout
        // questions render the exact same "every repo on this machine; saved to ..." text —
        // nothing in the E2E scenario can tell whose write-target line it saw, only that one of
        // them printed it. This is what actually pins that `ask_container_enabled` prints its
        // own, now that its wording can no longer prove it by itself.
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);

        ask_container_enabled(&mut io, &state, false).unwrap();

        let target_line = write_target_line(
            WriteScope::Global,
            state.global_config_path.as_deref().unwrap(),
            state.home_dir.as_deref(),
        );
        assert!(io.output.contains(&target_line), "{}", io.output);
    }

    #[test]
    fn agreeing_to_disable_containers_writes_false() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&["y"]);

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), Some(false));
    }

    #[test]
    fn an_already_disabled_host_defaults_to_staying_disabled() {
        let state = no_runtime(false, Source::Global);
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), None);
        assert!(io.output.contains("[Y/n]"), "{}", io.output);
        assert!(io.output.contains("(from your global config)"), "{}", io.output);
    }

    #[test]
    fn re_enabling_containers_writes_true() {
        let state = no_runtime(false, Source::Global);
        let mut io = ScriptedIo::new(&["n"]);

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), Some(true));
    }

    #[test]
    fn a_junk_answer_re_asks() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&["maybe", "y"]);

        assert_eq!(ask_container_enabled(&mut io, &state, false).unwrap(), Some(false));
        assert!(io.output.contains("Answer y or n"), "{}", io.output);
    }

    #[test]
    fn end_of_input_aborts_the_containers_question_too() {
        let state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[]);

        assert!(ask_container_enabled(&mut io, &state, false).is_err());
    }

    // ── The containers consent question ───────────────────────────────────────

    fn fresh(enabled: bool, source: Source, runtimes: Vec<container::RuntimeKind>) -> DetectedState {
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
            runtimes,
        )
    }

    #[test]
    fn consent_states_where_the_answer_will_be_saved_and_explains_itself() {
        let state = fresh(true, Source::CompiledDefault, vec![container::RuntimeKind::Podman]);
        let mut io = ScriptedIo::new(&[""]);

        ask_container_consent(&mut io, &state, false).unwrap();

        let target_line = write_target_line(
            WriteScope::Global,
            state.global_config_path.as_deref().unwrap(),
            state.home_dir.as_deref(),
        );
        assert!(io.output.contains(&target_line), "{}", io.output);
        assert!(io.output.contains("isolated filesystem"), "{}", io.output);
    }

    #[test]
    fn consent_notes_a_missing_runtime_without_blocking_the_choice() {
        let state = fresh(true, Source::CompiledDefault, Vec::new());
        let mut io = ScriptedIo::new(&[""]);

        let answer = ask_container_consent(&mut io, &state, false).unwrap();

        assert_eq!(answer, None, "accepting yes matches the compiled default");
        assert!(
            io.output.contains("no container runtime was found"),
            "{}",
            io.output
        );
    }

    #[test]
    fn consent_omits_the_missing_runtime_note_when_a_runtime_exists() {
        let state = fresh(true, Source::CompiledDefault, vec![container::RuntimeKind::Docker]);
        let mut io = ScriptedIo::new(&[""]);

        ask_container_consent(&mut io, &state, false).unwrap();

        assert!(
            !io.output.contains("no container runtime was found"),
            "{}",
            io.output
        );
        // And the two framings are mutually exclusive at the call site — this one must never
        // print the other question's failure-framed header.
        assert!(
            !io.output.contains("No container runtime found on this machine"),
            "{}",
            io.output
        );
    }

    #[test]
    fn accepting_the_consent_default_writes_nothing() {
        let state = fresh(true, Source::CompiledDefault, Vec::new());
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_container_consent(&mut io, &state, false).unwrap(), None);
    }

    #[test]
    fn declining_consent_writes_false() {
        let state = fresh(true, Source::CompiledDefault, Vec::new());
        let mut io = ScriptedIo::new(&["n"]);

        assert_eq!(
            ask_container_consent(&mut io, &state, false).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn consent_re_asks_on_a_junk_answer() {
        let state = fresh(true, Source::CompiledDefault, Vec::new());
        let mut io = ScriptedIo::new(&["maybe", "n"]);

        assert_eq!(
            ask_container_consent(&mut io, &state, false).unwrap(),
            Some(false)
        );
        assert!(io.output.contains("Answer y or n"), "{}", io.output);
    }

    #[test]
    fn end_of_input_aborts_the_consent_question() {
        let state = fresh(true, Source::CompiledDefault, Vec::new());
        let mut io = ScriptedIo::new(&[]);

        assert!(ask_container_consent(&mut io, &state, false).is_err());
    }

    #[test]
    fn consent_is_skipped_without_a_global_config_to_write_to() {
        let mut state = fresh(true, Source::CompiledDefault, Vec::new());
        state.global_config_path = None;
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(ask_container_consent(&mut io, &state, false).unwrap(), None);
        assert!(io.output.is_empty(), "{}", io.output);
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

        let (agent, enabled, ..) = resolve_effective(Some(&project), Some(&global));

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

        let (agent, enabled, ..) = resolve_effective(Some(&project), Some(&global));

        assert_eq!(agent.value, Some(KnownAgent::Claude));
        assert_eq!(agent.source, Source::Global);
        assert!(!enabled.value);
        assert_eq!(enabled.source, Source::Global);
    }

    #[test]
    fn compiled_defaults_apply_when_neither_file_sets_anything() {
        let (agent, enabled, ..) = resolve_effective(None, None);

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

        let (agent, enabled, ..) = resolve_effective(Some(&project), None);

        assert_eq!(agent.source, Source::CompiledDefault);
        assert_eq!(enabled.source, Source::CompiledDefault);
    }

    #[test]
    fn an_unparseable_file_reads_as_setting_nothing() {
        // doctor reports the parse error a few steps later; setup does not crash first.
        let tmp = TempDir::new().unwrap();
        let project = write(tmp.path(), "config.toml", "this is not = = toml\n");

        let (agent, ..) = resolve_effective(Some(&project), None);

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

        let (agent, enabled, ..) = resolve_effective(Some(&project), None);

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

        let (agent, enabled, ..) = resolve_effective(Some(&project), None);

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

        let (agent, ..) = resolve_effective(Some(&project), None);

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
        let (agent, ..) = resolve_effective(Some(&path), None);
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

        let (_, enabled, ..) = resolve_effective(None, Some(&path));
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

    // ── Skeleton cleanup ────────────────────────────────────────────────────

    #[test]
    fn the_skeleton_example_lines_match_render_global_config_skeleton_verbatim() {
        let skeleton = render_global_config_skeleton();
        for line in [
            TMUX_AGENT_PANE_EXAMPLE_LINE,
            TMUX_SPLIT_EXAMPLE_LINE,
            TMUX_SPLIT_PERCENT_EXAMPLE_LINE,
            CONTAINER_ENABLED_EXAMPLE_LINE,
        ] {
            assert!(
                skeleton.lines().any(|l| l == line),
                "{line:?} is not a verbatim line of render_global_config_skeleton()"
            );
        }
    }

    #[test]
    fn an_interactively_activated_project_agent_leaves_no_duplicate_commented_example() {
        // The exact sequence `cmd_setup` runs on the interactive path: a plain skeleton
        // (nothing known yet), then the answer written afterward.
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_project_config_skeleton());

        assert!(update_project_agent(&path, KnownAgent::Claude).unwrap());
        strip_project_agent_example(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent = \"claude\""), "{after}");
        assert!(
            !after.contains(AGENT_EXAMPLE_LINE),
            "the commented example must not survive next to the active line: {after}"
        );
    }

    #[test]
    fn stripping_the_project_agent_example_is_a_no_op_when_it_was_never_written() {
        // The flag/`--yes` path renders the active line directly at creation (see
        // `render_project_config_skeleton_with_agent`), so the commented example was never
        // in the file to begin with — stripping it must be a harmless no-op.
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "config.toml",
            &render_project_config_skeleton_with_agent(KnownAgent::Claude),
        );
        let before = std::fs::read_to_string(&path).unwrap();

        strip_project_agent_example(&path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn an_interactively_activated_container_enabled_leaves_no_duplicate_commented_example() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        assert!(update_global_container_enabled(&path, false).unwrap());
        strip_global_container_enabled_example(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("enabled = false"), "{after}");
        assert!(
            !after.contains(CONTAINER_ENABLED_EXAMPLE_LINE),
            "the commented example must not survive next to the active line: {after}"
        );
    }

    #[test]
    fn an_interactively_activated_layout_leaves_no_duplicate_commented_examples() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            30,
        )
        .unwrap();
        assert_eq!(written, vec!["agent_pane", "split", "split_percent"]);
        strip_global_tmux_layout_examples(&path, &written).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent_pane = \"right\""), "{after}");
        assert!(after.contains("split = \"vertical\""), "{after}");
        assert!(after.contains("split_percent = 30"), "{after}");
        for example in [
            TMUX_AGENT_PANE_EXAMPLE_LINE,
            TMUX_SPLIT_EXAMPLE_LINE,
            TMUX_SPLIT_PERCENT_EXAMPLE_LINE,
        ] {
            assert!(
                !after.contains(example),
                "no commented example may survive next to the active line it duplicates: {after}"
            );
        }
    }

    #[test]
    fn strip_global_tmux_layout_examples_only_strips_the_keys_named() {
        // A bare skeleton has no active tmux keys at all, so `update_global_tmux_layout`
        // always writes all three the first time any of them changes (there is nothing yet
        // to compare a single key against) — the case where only one key's example needs
        // removing happens on a file that already has the *other* two active, which is
        // exactly what `tmux_layout_writes_only_the_key_that_actually_changed` covers above.
        // This test pins `strip_global_tmux_layout_examples`'s own per-key selectivity
        // directly: only the keys named in `written_keys` lose their commented example.
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        strip_global_tmux_layout_examples(&path, &["split_percent"]).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(TMUX_SPLIT_PERCENT_EXAMPLE_LINE), "{after}");
        assert!(after.contains(TMUX_AGENT_PANE_EXAMPLE_LINE), "{after}");
        assert!(after.contains(TMUX_SPLIT_EXAMPLE_LINE), "{after}");
    }

    #[test]
    fn strip_skeleton_example_is_a_no_op_when_the_line_is_not_present() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", "[tmux]\nagent_pane = \"left\"\n");
        let before = std::fs::read_to_string(&path).unwrap();

        strip_skeleton_example(&path, TMUX_AGENT_PANE_EXAMPLE_LINE).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    // The boundary case that removed the old "only a file this run created" gate: the project
    // config may equally well come from `am init` in an earlier invocation, an older `am`, or
    // one a teammate committed — `cmd_setup` no longer distinguishes, and neither do these two
    // functions. Byte-exact identity to our own skeleton line is what makes that safe; the
    // near-miss test right after these two is what pins that boundary.

    #[test]
    fn a_config_from_an_earlier_am_init_invocation_gets_its_stale_agent_example_removed() {
        // `config::write_defaults` — not `render_project_config_skeleton` directly — is the
        // actual function `am init` calls, so this is genuinely "a file `am init` wrote",
        // simulating it having happened in a prior, unrelated invocation of `am`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        config::write_defaults(&path).unwrap();

        assert!(update_project_agent(&path, KnownAgent::Claude).unwrap());
        strip_project_agent_example(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent = \"claude\""), "{after}");
        assert!(
            !after.contains(AGENT_EXAMPLE_LINE),
            "a stale example must be removed even though this run didn't write the file: {after}"
        );
    }

    #[test]
    fn a_config_from_an_earlier_invocation_gets_its_stale_container_enabled_example_removed() {
        // Global config has no separate "am init"-style writer the way the project file does
        // (there is no `am init` equivalent for it) — `render_global_config_skeleton` is the
        // only skeleton it's ever written from, whether that happened moments ago or in an
        // earlier `am setup` invocation. This models the latter: the skeleton already sitting
        // on disk before this run touches anything.
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        assert!(update_global_container_enabled(&path, false).unwrap());
        strip_global_container_enabled_example(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("enabled = false"), "{after}");
        assert!(
            !after.contains(CONTAINER_ENABLED_EXAMPLE_LINE),
            "a stale example must be removed even though this run didn't write the file: {after}"
        );
    }

    #[test]
    fn a_config_from_an_earlier_invocation_gets_all_three_stale_layout_examples_removed() {
        // Same rationale as the container.enabled case above, but forcing all three tmux keys
        // to actually change (unlike the split/split_percent-only test below, which exists to
        // pin per-key selectivity) so each of the three example lines is proven removable, not
        // just two of three.
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            40,
        )
        .unwrap();
        assert_eq!(written, vec!["agent_pane", "split", "split_percent"]);
        strip_global_tmux_layout_examples(&path, &written).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent_pane = \"right\""), "{after}");
        assert!(after.contains("split = \"vertical\""), "{after}");
        assert!(after.contains("split_percent = 40"), "{after}");
        for example in [
            TMUX_AGENT_PANE_EXAMPLE_LINE,
            TMUX_SPLIT_EXAMPLE_LINE,
            TMUX_SPLIT_PERCENT_EXAMPLE_LINE,
        ] {
            assert!(
                !after.contains(example),
                "a stale example must be removed even though this run didn't write the file: {after}"
            );
        }
    }

    #[test]
    fn a_config_from_an_earlier_invocation_gets_its_stale_layout_examples_removed() {
        // `agent_pane` is already live here — as if an earlier interactive `am setup` run had
        // already activated it — while `split`/`split_percent` are still at their commented
        // skeleton defaults, exactly what a later run that only touches the percentage sees.
        let tmp = TempDir::new().unwrap();
        let content = "[tmux]\nagent_pane = \"left\"\n\
            # split = \"horizontal\"   # split direction: \"horizontal\" | \"vertical\"\n\
            # split_percent = 50     # percentage of the window given to the agent pane (1-99)\n";
        let path = write(tmp.path(), "config.toml", content);

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Left,
            config::SplitDirection::Vertical,
            40,
        )
        .unwrap();
        assert_eq!(written, vec!["split", "split_percent"]);
        strip_global_tmux_layout_examples(&path, &written).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("split = \"vertical\""), "{after}");
        assert!(after.contains("split_percent = 40"), "{after}");
        assert!(
            !after.contains(TMUX_SPLIT_EXAMPLE_LINE) && !after.contains(TMUX_SPLIT_PERCENT_EXAMPLE_LINE),
            "stale examples must be removed even though this run didn't write the file: {after}"
        );
    }

    #[test]
    fn a_near_miss_example_line_survives_because_it_is_not_byte_exact() {
        // The whole safety story, now that stripping is no longer gated on file freshness:
        // only a line that is byte-for-byte identical to our own skeleton's is ever removed.
        // A line that merely resembles it — different spacing, a different trailing comment,
        // one a user deliberately kept — is not our text and must never be touched. A future
        // refactor toward a looser match (e.g. "starts with `# agent =`") would fail this.
        let tmp = TempDir::new().unwrap();
        let content =
            "[defaults]\nagent = \"claude\"\n# agent = \"claude\"   # example I kept deliberately\n";
        let path = write(tmp.path(), "config.toml", content);

        strip_project_agent_example(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            content,
            "a near-miss commented line must never be stripped"
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

    // ── The write-target line ─────────────────────────────────────────────────

    #[test]
    fn write_target_line_shortens_a_project_path_relative_to_the_repo_root() {
        let project = Path::new("/repo/.am/config.toml");
        assert_eq!(
            write_target_line(WriteScope::Project, project, Some(Path::new("/repo"))),
            "just this repo; saved to .am/config.toml."
        );
    }

    #[test]
    fn write_target_line_shortens_a_global_path_to_a_tilde() {
        let global = Path::new("/home/u/.config/am/config.toml");
        assert_eq!(
            write_target_line(WriteScope::Global, global, Some(Path::new("/home/u"))),
            "every repo on this machine; saved to ~/.config/am/config.toml."
        );
    }

    #[test]
    fn write_target_line_falls_back_to_the_absolute_path_without_a_known_base() {
        let project = Path::new("/repo/.am/config.toml");
        assert_eq!(
            write_target_line(WriteScope::Project, project, None),
            "just this repo; saved to /repo/.am/config.toml."
        );

        let global = Path::new("/home/u/.config/am/config.toml");
        assert_eq!(
            write_target_line(WriteScope::Global, global, None),
            "every repo on this machine; saved to /home/u/.config/am/config.toml."
        );
    }

    #[test]
    fn write_target_line_falls_back_to_the_absolute_path_when_not_under_the_base() {
        // A path that is not actually under `base` (e.g. XDG_CONFIG_HOME pointed somewhere
        // outside $HOME) must not be silently mis-shortened.
        let global = Path::new("/mnt/config/am/config.toml");
        assert_eq!(
            write_target_line(WriteScope::Global, global, Some(Path::new("/home/u"))),
            "every repo on this machine; saved to /mnt/config/am/config.toml."
        );
    }

    #[test]
    fn tilde_shorten_matches_a_global_write_target_lines_own_abbreviation() {
        // `cmd_setup`'s header line uses this directly so the repo path it names is
        // abbreviated exactly the same way a global write-target line's path is — one
        // convention, not two.
        let path = Path::new("/home/u/src/agent-manager");
        assert_eq!(
            tilde_shorten(path, Some(Path::new("/home/u"))),
            Path::new("~/src/agent-manager")
        );
        assert_eq!(tilde_shorten(path, None), path);
        assert_eq!(
            tilde_shorten(path, Some(Path::new("/mnt/other"))),
            path,
            "a path outside the given home must not be mis-shortened"
        );
    }

    // ── Dim rendering ─────────────────────────────────────────────────────────

    #[test]
    fn dim_line_indents_always_and_paints_only_when_color_is_enabled() {
        // The pty/color hazard this covers: a real pty makes `color::enabled` true for `am
        // setup`'s cucumber scenarios, so this has to be checked without one — see the
        // hazard note on `AmWorld::run_am_pty` in `tests/cucumber.rs`. Covers both `am
        // setup`'s own secondary lines and, since `dim_line` is shared with `main.rs`, the
        // init report's `Status` lines and the "Created ..." global-config line too.
        assert_eq!(dim_line("hello", false), "  hello");
        assert_eq!(dim_line("hello", true), "\x1b[2m  hello\x1b[0m");
    }

    #[test]
    fn colored_rendering_dims_the_write_target_and_currently_lines() {
        // The pty/color hazard this covers: `ScriptedIo`-based tests never see ANSI codes
        // unless `color: true` is passed explicitly, the same way the interactive cucumber
        // scenarios only see them because a real pty makes `color::enabled` true there too.
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[""]);

        ask_agent(&mut io, &state, None, true).unwrap();

        assert!(
            io.output.contains("\x1b[2m  just this repo; saved to"),
            "{}",
            io.output
        );
        assert!(io.output.contains("\x1b[2m  currently:"), "{}", io.output);
        // The menu itself is content, not structure — it must never be dimmed.
        assert!(!io.output.contains("\x1b[2m  [1]"), "{}", io.output);
    }

    // ── The pane layout question ──────────────────────────────────────────────

    /// A `DetectedState` whose three tmux effective values are set independently of the
    /// agent/container ones `configured` already covers.
    fn layout_state(
        agent_pane: config::PaneSide,
        split: config::SplitDirection,
        split_percent: u8,
        source: Source,
    ) -> DetectedState {
        let mut state = configured(Some(KnownAgent::Claude), Source::Project);
        state.effective_tmux_agent_pane = Effective {
            value: agent_pane,
            source,
        };
        state.effective_tmux_split = Effective { value: split, source };
        state.effective_tmux_split_percent = Effective {
            value: split_percent,
            source,
        };
        state
    }

    #[test]
    fn layout_is_not_asked_about_without_a_global_config_to_write_to() {
        let mut state = configured(Some(KnownAgent::Claude), Source::Project);
        state.global_config_path = None;
        let mut io = ScriptedIo::new(&[]);

        assert_eq!(ask_layout(&mut io, &state, false).unwrap(), None);
        assert!(io.output.is_empty(), "{}", io.output);
    }

    #[test]
    fn ask_layout_returns_none_when_the_choice_matches_every_effective_value() {
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::Global,
        );
        // Enter accepts preset 1, which is exactly `am`'s compiled default and also this
        // state's effective triple.
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_layout(&mut io, &state, false).unwrap(), None);
    }

    #[test]
    fn ask_layout_returns_some_when_the_choice_differs() {
        let state = layout_state(
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            30,
            Source::Global,
        );
        // Explicitly picking a preset that differs from the effective triple (right/vertical/
        // 30 is not any of the four presets) must return `Some` — the write-if-changed half of
        // the contract, exercised via an explicit choice rather than Enter.
        let mut io = ScriptedIo::new(&["1"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some(LAYOUT_PRESETS[0].clone())
        );
    }

    #[test]
    fn ask_layout_enter_resolves_to_the_effective_triple_and_writes_nothing() {
        // This is the exact scenario the reported bug lost data on: a customized, non-preset-1
        // layout (agent_pane=right, split=vertical, split_percent=30) with a global config
        // already set to it. Pressing Enter must resolve to *that* effective triple, not
        // `LAYOUT_PRESETS[0]` — and because the resolved triple then exactly matches what's
        // already effective, `ask_layout` must report `None` (nothing to write), never `Some`
        // of a hardcoded preset that would silently clobber the customization on disk.
        let state = layout_state(
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            30,
            Source::Global,
        );
        let mut io = ScriptedIo::new(&[""]);

        assert_eq!(ask_layout(&mut io, &state, false).unwrap(), None);
    }

    #[test]
    fn preset_selection_by_number_returns_the_fixed_triple() {
        let state = layout_state(
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            30,
            Source::Global,
        );

        for (index, preset) in LAYOUT_PRESETS.iter().enumerate() {
            let answer = (index + 1).to_string();
            let mut io = ScriptedIo::new(&[answer.as_str()]);

            assert_eq!(
                ask_layout(&mut io, &state, false).unwrap(),
                Some(preset.clone()),
                "preset {}",
                index + 1
            );
        }
    }

    #[test]
    fn invalid_layout_selection_re_asks_with_a_reason() {
        let state = layout_state(
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            30,
            Source::Global,
        );
        let mut io = ScriptedIo::new(&["9", "nope", "1"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some(LAYOUT_PRESETS[0].clone())
        );
        assert_eq!(io.output.matches("is not one of 1-5").count(), 2, "{}", io.output);
    }

    #[test]
    fn customize_words_the_pane_question_for_a_horizontal_split() {
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        let mut io = ScriptedIo::new(&["5", "1", "2", "60", "y"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some((
                config::PaneSide::Right,
                config::SplitDirection::Horizontal,
                60
            ))
        );
        assert!(
            io.output.contains("Which side should the agent be on?"),
            "{}",
            io.output
        );
        assert!(io.output.contains("[1] left"), "{}", io.output);
        assert!(io.output.contains("[2] right"), "{}", io.output);
        assert!(
            !io.output.contains("top or on the bottom"),
            "a horizontal split must not ask about top/bottom: {}",
            io.output
        );
    }

    #[test]
    fn customize_words_the_pane_question_for_a_vertical_split() {
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        let mut io = ScriptedIo::new(&["5", "2", "1", "40", "y"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some((
                config::PaneSide::Left,
                config::SplitDirection::Vertical,
                40
            ))
        );
        assert!(
            io.output.contains("Should the agent be on top or on the bottom?"),
            "{}",
            io.output
        );
        assert!(io.output.contains("[1] top"), "{}", io.output);
        assert!(io.output.contains("[2] bottom"), "{}", io.output);
        assert!(
            !io.output.contains("Which side should the agent be on?"),
            "a vertical split must not ask left/right: {}",
            io.output
        );
    }

    #[test]
    fn customize_preview_states_the_proportion_in_text_for_a_vertical_split() {
        // A stacked preview can't show proportion in its ASCII (both boxes render equal size
        // regardless of `percent`), so the confirm step must not be based on the picture
        // alone — a skewed 95/5 split is exactly the case that would otherwise mislead.
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        let mut io = ScriptedIo::new(&["5", "2", "1", "95", "y"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some((
                config::PaneSide::Left,
                config::SplitDirection::Vertical,
                95
            ))
        );
        assert!(
            io.output.contains("agent (top) gets 95%, the other pane gets 5%."),
            "{}",
            io.output
        );
    }

    #[test]
    fn customize_preview_states_the_proportion_in_text_for_a_horizontal_split_too() {
        // Applied consistently: a horizontal preview's box width already conveys proportion,
        // but the same text line is shown there too so the wording doesn't differ between the
        // two directions.
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        let mut io = ScriptedIo::new(&["5", "1", "2", "60", "y"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some((
                config::PaneSide::Right,
                config::SplitDirection::Horizontal,
                60
            ))
        );
        assert!(
            io.output.contains("agent (right) gets 60%, the other pane gets 40%."),
            "{}",
            io.output
        );
    }

    #[test]
    fn declining_the_customize_preview_re_shows_the_preset_menu() {
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        // Enter customize, accept every default, decline the preview, then pick preset 2
        // from the (re-shown) preset menu.
        let mut io = ScriptedIo::new(&["5", "", "", "", "n", "2"]);

        assert_eq!(
            ask_layout(&mut io, &state, false).unwrap(),
            Some((
                config::PaneSide::Right,
                config::SplitDirection::Horizontal,
                50
            ))
        );
        assert_eq!(
            io.output.matches("Which layout do you want?").count(),
            2,
            "the preset menu must be shown again after declining: {}",
            io.output
        );
        let target_line = write_target_line(
            WriteScope::Global,
            state.global_config_path.as_deref().unwrap(),
            state.home_dir.as_deref(),
        );
        assert_eq!(
            io.output.matches(target_line.as_str()).count(),
            2,
            "the write-target line must be shown again after declining: {}",
            io.output
        );
    }

    // ── Blank-line rhythm ─────────────────────────────────────────────────────
    //
    // A pty scenario cannot reliably pin this: a `script`-driven capture (see the hazard note
    // on `AmWorld::run_am_pty` in `tests/cucumber.rs`) has been observed to drop a blank line
    // that immediately follows a prompt's echoed answer, non-deterministically, depending on
    // which prompt in the run happens to be first — a `script`/pty-relay timing artifact, not
    // a defect in what the program actually writes. `ScriptedIo` captures the literal bytes
    // `am` produces, with no terminal in the loop to race against, which is what makes it the
    // trustworthy guard here.

    #[test]
    fn every_top_level_question_opens_with_a_blank_line() {
        let agent_state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[""]);
        ask_agent(&mut io, &agent_state, None, false).unwrap();
        assert!(
            io.output.starts_with("\nWhich agent do you use?\n"),
            "{}",
            io.output
        );

        let container_state = no_runtime(true, Source::CompiledDefault);
        let mut io = ScriptedIo::new(&[""]);
        ask_container_enabled(&mut io, &container_state, false).unwrap();
        assert!(
            io.output.starts_with("\nNo container runtime found"),
            "{}",
            io.output
        );

        let the_layout_state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::Global,
        );
        let mut io = ScriptedIo::new(&[""]);
        ask_layout(&mut io, &the_layout_state, false).unwrap();
        assert!(
            io.output.starts_with("\nWhich layout do you want?\n"),
            "{}",
            io.output
        );
    }

    #[test]
    fn every_customize_sub_question_opens_with_a_blank_line() {
        let state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::CompiledDefault,
        );
        let mut io = ScriptedIo::new(&["5", "1", "2", "60", "y"]);

        ask_layout(&mut io, &state, false).unwrap();

        // Each sub-question's own header is immediately preceded by a blank line — asserted as
        // an ordered "\n\n<header>" substring rather than a fixed offset, since the preset menu
        // printed above them isn't a fixed size.
        for header in [
            "\nSide by side, or stacked?\n",
            "\nWhich side should the agent be on?\n",
            "\nWhat percentage of the window should the agent pane get?\n",
            "\nHere's what that looks like:\n",
        ] {
            assert!(
                io.output.contains(header),
                "missing {header:?} in {}",
                io.output
            );
        }
    }

    #[test]
    fn invalid_customize_direction_re_asks() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["5", "9", "1", "", "", "y"]);

        ask_layout(&mut io, &state, false).unwrap();

        assert!(io.output.contains("'9' is not 1 or 2."), "{}", io.output);
    }

    #[test]
    fn invalid_customize_percent_re_asks() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["5", "", "", "150", "50", "y"]);

        ask_layout(&mut io, &state, false).unwrap();

        assert!(
            io.output.contains("must be a number between 1 and 99"),
            "{}",
            io.output
        );
    }

    #[test]
    fn end_of_input_aborts_the_layout_question() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&[]);

        assert!(ask_layout(&mut io, &state, false).is_err());
    }

    #[test]
    fn end_of_input_aborts_the_customize_direction_question() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["5"]);

        assert!(ask_layout(&mut io, &state, false).is_err());
    }

    #[test]
    fn end_of_input_aborts_the_customize_pane_side_question() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["5", ""]);

        assert!(ask_layout(&mut io, &state, false).is_err());
    }

    #[test]
    fn end_of_input_aborts_the_customize_percent_question() {
        let state = configured(Some(KnownAgent::Claude), Source::Project);
        let mut io = ScriptedIo::new(&["5", "", ""]);

        assert!(ask_layout(&mut io, &state, false).is_err());
    }

    #[test]
    fn the_project_override_caveat_is_shown_only_when_a_project_value_exists() {
        let mut state = layout_state(
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
            Source::Global,
        );
        let mut io = ScriptedIo::new(&[""]);
        ask_layout(&mut io, &state, false).unwrap();
        assert!(
            !io.output.contains("already sets its own"),
            "no project override, no caveat: {}",
            io.output
        );

        state.effective_tmux_split_percent.source = Source::Project;

        // Uncolored: the caveat line is exactly the prefix + a space + the message, not just
        // a substring of the message — a dropped `note_prefix()` call must fail this.
        let mut io = ScriptedIo::new(&[""]);
        ask_layout(&mut io, &state, false).unwrap();
        let expected_line = format!(
            "{} {PROJECT_LAYOUT_OVERRIDE_NOTE}",
            color::note_prefix(false)
        );
        assert!(
            io.output.contains(&expected_line),
            "expected line {expected_line:?} in {}",
            io.output
        );

        // Colored: the escape codes wrap the "Note:" label alone, and the message that
        // follows is untouched plain text — same shape `note_prefix`'s own unit tests pin,
        // asserted here at the call site so this path isn't left unexercised.
        let mut io = ScriptedIo::new(&[""]);
        ask_layout(&mut io, &state, true).unwrap();
        let expected_colored_line = format!(
            "{} {PROJECT_LAYOUT_OVERRIDE_NOTE}",
            color::note_prefix(true)
        );
        assert!(
            io.output.contains(&expected_colored_line),
            "expected line {expected_colored_line:?} in {}",
            io.output
        );
        assert!(io.output.contains("\x1b[33mNote:\x1b[0m"), "{}", io.output);
    }

    // ── render_layout ─────────────────────────────────────────────────────────

    #[test]
    fn render_layout_matches_each_fixed_preset_exactly() {
        assert_eq!(
            render_layout(
                &config::PaneSide::Left,
                &config::SplitDirection::Horizontal,
                50
            ),
            vec!["[  agent   |  shell   ]".to_string()]
        );
        assert_eq!(
            render_layout(
                &config::PaneSide::Right,
                &config::SplitDirection::Horizontal,
                50
            ),
            vec!["[  shell   |  agent   ]".to_string()]
        );
        assert_eq!(
            render_layout(
                &config::PaneSide::Left,
                &config::SplitDirection::Horizontal,
                70
            ),
            vec!["[    agent    | shell ]".to_string()]
        );
        assert_eq!(
            render_layout(
                &config::PaneSide::Left,
                &config::SplitDirection::Vertical,
                50
            ),
            vec![
                "[       agent        ]".to_string(),
                "[       shell        ]".to_string(),
            ]
        );
    }

    #[test]
    fn render_layout_degrades_sensibly_at_an_extreme_percentage() {
        let lines = render_layout(
            &config::PaneSide::Left,
            &config::SplitDirection::Horizontal,
            95,
        );
        assert_eq!(lines.len(), 1);
        // Neither label is ever squeezed below its own length, however lopsided the split.
        assert!(lines[0].contains(" agent "), "{lines:?}");
        assert!(lines[0].contains(" shell "), "{lines:?}");
    }

    // ── update_global_tmux_layout ─────────────────────────────────────────────

    #[test]
    fn tmux_layout_matching_the_file_already_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = write(
            tmp.path(),
            "config.toml",
            "[tmux]\nagent_pane = \"left\"\nsplit = \"horizontal\"\nsplit_percent = 50\n",
        );
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            50,
        )
        .unwrap();

        assert!(written.is_empty(), "{written:?}");
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn tmux_layout_writes_only_the_key_that_actually_changed() {
        let tmp = TempDir::new().unwrap();
        let content =
            "[tmux]\nagent_pane = \"left\"   # keep me\nsplit = \"horizontal\"\nsplit_percent = 50\n";
        let path = write(tmp.path(), "config.toml", content);

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Left,
            config::SplitDirection::Horizontal,
            70,
        )
        .unwrap();

        assert_eq!(written, vec!["split_percent"]);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("agent_pane = \"left\"   # keep me"), "{after}");
        assert!(after.contains("split = \"horizontal\""), "{after}");
        assert!(after.contains("split_percent = 70"), "{after}");
    }

    #[test]
    fn tmux_layout_is_added_to_a_skeleton_global_file() {
        let tmp = TempDir::new().unwrap();
        let path = write(tmp.path(), "config.toml", render_global_config_skeleton());

        let written = update_global_tmux_layout(
            &path,
            config::PaneSide::Right,
            config::SplitDirection::Vertical,
            80,
        )
        .unwrap();

        assert_eq!(written, vec!["agent_pane", "split", "split_percent"]);
        let (_, _, agent_pane, split, split_percent) = resolve_effective(None, Some(&path));
        assert_eq!(agent_pane.value, config::PaneSide::Right);
        assert_eq!(split.value, config::SplitDirection::Vertical);
        assert_eq!(split_percent.value, 80);
    }
}
