//! `am doctor` — report what is and is not ready for a successful `am start`.
//!
//! Every check answers one question a user would otherwise answer by running `am start`
//! and reading a failure. The checks deliberately reuse the same functions `cmd_start`
//! calls (`detect_runtime`, `validate_agent_credentials`, `devcontainer::find_config`),
//! so a passing report and a working `am start` cannot drift apart.
//!
//! Nothing here mutates anything. `am doctor` is safe to run at any time, which is the
//! point: it is the alternative to `am start` silently bootstrapping state as a side
//! effect of being run.

use std::path::Path;

use crate::color::{paint, Color};
use crate::config::{Config, ContainerMode, Vcs};
use crate::{config, container, devcontainer, tmux};

// ── Report types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Ready.
    Ok,
    /// Usable, but something is worth knowing.
    Warn,
    /// `am start` will not succeed in this configuration.
    Fail,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        }
    }

    fn color(self) -> Color {
        match self {
            Status::Ok => Color::Green,
            Status::Warn => Color::Yellow,
            Status::Fail => Color::Red,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub section: &'static str,
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What to do about it. Only meaningful for `Warn` and `Fail`.
    pub hint: Option<String>,
}

impl Check {
    fn new(section: &'static str, name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self {
            section,
            name: name.into(),
            status,
            detail: detail.into(),
            hint: None,
        }
    }

    fn ok(section: &'static str, name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(section, name, Status::Ok, detail)
    }

    fn warn(
        section: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::new(section, name, Status::Warn, detail).with_hint(hint)
    }

    fn fail(
        section: &'static str,
        name: impl Into<String>,
        detail: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self::new(section, name, Status::Fail, detail).with_hint(hint)
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count()
    }

    /// Render for a terminal, grouped by section in the order the checks were added.
    ///
    /// `color` is passed in rather than probed here so rendering stays pure and both forms
    /// are testable. Only the status glyph is colored: the alignment columns depend on
    /// the visible width of what precedes them, and ANSI codes have none — coloring the
    /// name or detail would push every escape sequence into the padding arithmetic.
    pub fn render(&self, color: bool) -> String {
        let mut out = String::new();
        let mut current_section: Option<&str> = None;
        for check in &self.checks {
            if current_section != Some(check.section) {
                if current_section.is_some() {
                    out.push('\n');
                }
                out.push_str(check.section);
                out.push('\n');
                current_section = Some(check.section);
            }
            out.push_str(&format!(
                "  {} {:<22} {}\n",
                paint(check.status.glyph(), check.status.color(), color),
                check.name,
                check.detail
            ));
            if let Some(ref hint) = check.hint {
                // Indented under the check it belongs to, so a wall of hints cannot be
                // mistaken for a list of separate problems. Dimmed for the same reason:
                // the finding is what you scan for, the hint is what you read once you
                // have found it.
                out.push_str(&paint(
                    &format!("      → {hint}"),
                    Color::Dim,
                    color,
                ));
                out.push('\n');
            }
        }

        // The verdict carries the same severity as the worst check, so it takes the same
        // color — a red summary line is what you see when the report scrolls past.
        let (failures, warnings) = (self.failures(), self.warnings());
        out.push('\n');
        if failures > 0 {
            let noun = if failures == 1 { "problem" } else { "problems" };
            out.push_str(&paint(
                &format!("{failures} {noun} will prevent 'am start' from working."),
                Color::Red,
                color,
            ));
            out.push('\n');
        } else if warnings > 0 {
            out.push_str(&paint(
                "Ready. Some notes above are worth reading.",
                Color::Yellow,
                color,
            ));
            out.push('\n');
        } else {
            out.push_str(&paint("Ready.", Color::Green, color));
            out.push('\n');
        }
        out
    }
}

// ── Checks ────────────────────────────────────────────────────────────────────

const REPO: &str = "Repository";
const PROJECT: &str = "Project setup";
const TMUX: &str = "tmux";
const RUNTIME: &str = "Container runtime";
const ENVIRONMENT: &str = "Environment";
const AGENT: &str = "Agent";

/// Build the full report.
///
/// `repo` is `None` when the current directory is not inside a repository; that is a
/// finding to report rather than an error to return, since it is one of the things a user
/// runs `doctor` to discover.
pub fn run(repo: Option<(&Path, Vcs)>, agent_flag: Option<&str>) -> Report {
    let mut report = Report::default();

    let repo_root = check_repository(&mut report, repo);
    let cfg = check_config(&mut report, repo_root);

    if let Some(root) = repo_root {
        if let Ok(cwd) = std::env::current_dir() {
            check_shadowed_config(&mut report, &cwd, root);
        }
        check_project_setup(&mut report, root, &cfg);
    }
    check_tmux(&mut report);

    let runtime = check_runtime(&mut report, &cfg);
    check_ssh_agent(&mut report, &cfg);
    let agent_name = effective_agent(agent_flag, &cfg);

    if let Some(root) = repo_root {
        check_environment(&mut report, root, &cfg, runtime.as_ref(), agent_name.as_deref());
    }
    check_agent(&mut report, agent_name.as_deref());

    report
}

/// Load config the same way `cmd_start` does, falling back to global-only outside a repo.
///
/// A config that fails to load is reported rather than swallowed. `doctor` used to fall
/// back to compiled-in defaults silently, which is the worst possible answer here: every
/// later check would describe a configuration the user does not have, and the one command
/// meant to explain the problem would be the one hiding it. `am start` fails outright on
/// the same file, so a clean report would have been a lie.
/// Describe which config files were actually read.
///
/// "loaded" was ambiguous in the one case where it mattered: a user editing a config that
/// `am` never opens reads it as confirmation. Naming the files turns that into an obvious
/// mismatch — the path on screen is not the path they edited. Files that do not exist are
/// listed as absent rather than omitted, since "no project config" is itself the answer to
/// why a setting had no effect.
fn sources(global: Option<&Path>, project: Option<&Path>) -> String {
    let mut parts = Vec::new();
    match global.filter(|p| p.exists()) {
        Some(p) => parts.push(format!("global {}", p.display())),
        None => parts.push("no global config".to_string()),
    }
    match project {
        Some(p) if p.exists() => parts.push(format!("project {}", p.display())),
        Some(p) => parts.push(format!("no project config at {}", p.display())),
        None => {}
    }
    parts.join(", ")
}

fn check_config(report: &mut Report, repo_root: Option<&Path>) -> Config {
    let project = repo_root.map(|r| r.join(".am").join("config.toml"));
    let global = config::global_config_path();
    match config::load_with_global(global.as_deref(), project.as_deref()) {
        Ok(cfg) => {
            if cfg.unknown_keys.is_empty() {
                report
                    .checks
                    .push(Check::ok(PROJECT, "config", sources(global.as_deref(), project.as_deref())));
            } else {
                // Named individually: "3 unknown keys" tells the user they have a
                // problem without telling them where, which is the part that is hard
                // to work out from a file they believe is correct. Grouped by file so
                // the path is not repeated once per key — with two config files in
                // play, which file a key came from is the whole question.
                let mut groups: Vec<(&Path, Vec<&str>)> = Vec::new();
                for unknown in &cfg.unknown_keys {
                    match groups.last_mut() {
                        Some((file, keys)) if *file == unknown.file => {
                            keys.push(&unknown.key)
                        }
                        _ => groups.push((&unknown.file, vec![&unknown.key])),
                    }
                }
                let list = groups
                    .iter()
                    .map(|(file, keys)| format!("{} in {}", keys.join(", "), file.display()))
                    .collect::<Vec<_>>()
                    .join("; ");
                report.checks.push(Check::warn(
                    PROJECT,
                    "config",
                    format!("loaded, with unrecognised keys: {list}"),
                    "remove them or correct the spelling — they have no effect",
                ));
            }
            cfg
        }
        Err(e) => {
            report.checks.push(Check::fail(
                PROJECT,
                "config",
                // The chain names the offending file and key; without it the user gets
                // "invalid config" and no way to find which of two files is at fault.
                format!("{e:#}"),
                "fix the reported file, or move it aside to fall back to defaults",
            ));
            Config::default()
        }
    }
}

fn effective_agent(agent_flag: Option<&str>, cfg: &Config) -> Option<String> {
    agent_flag.map(str::to_string).or_else(|| cfg.agent.clone())
}

/// Warn about a project config sitting between the current directory and the repo root.
///
/// `.am/config.toml` is meant to be committed, so a copy appears in every session
/// worktree — but `find_repo_root` deliberately walks past worktrees and jj workspaces
/// to the main repository, and only that copy is ever read. Editing the file in front of
/// you therefore does nothing at all, with no indication of why. Naming both paths is the
/// whole fix: the file that is ignored, and the one to edit instead.
fn check_shadowed_config(report: &mut Report, cwd: &Path, repo_root: &Path) {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d == repo_root {
            return;
        }
        let shadowed = d.join(".am").join("config.toml");
        if shadowed.exists() {
            report.checks.push(Check::warn(
                PROJECT,
                "shadowed config",
                format!("{} is never read", shadowed.display()),
                format!(
                    "am uses the repository root's config — edit {} instead",
                    repo_root.join(".am").join("config.toml").display()
                ),
            ));
            return;
        }
        dir = d.parent();
    }
}

fn check_repository<'a>(report: &mut Report, repo: Option<(&'a Path, Vcs)>) -> Option<&'a Path> {
    match repo {
        Some((root, vcs)) => {
            let kind = match vcs {
                Vcs::Git => "git",
                Vcs::Jj => "jj",
            };
            report.checks.push(Check::ok(
                REPO,
                "version control",
                format!("{kind} repository at {}", root.display()),
            ));
            Some(root)
        }
        None => {
            report.checks.push(Check::fail(
                REPO,
                "version control",
                "not inside a git or jj repository",
                "run 'am' from inside a repository, or create one with 'git init'",
            ));
            None
        }
    }
}

fn check_project_setup(report: &mut Report, repo_root: &Path, cfg: &Config) {
    let am_dir = repo_root.join(".am");
    if !am_dir.is_dir() {
        report.checks.push(Check::fail(
            PROJECT,
            ".am/",
            "not initialized",
            "run 'am init'",
        ));
        return;
    }
    report.checks.push(Check::ok(
        PROJECT,
        ".am/",
        format!("initialized at {}", am_dir.display()),
    ));

    // am start generates a gitconfig from git config at session start time.
    // Check that git identity is available so the generated gitconfig will be useful.
    let has_name = std::process::Command::new("git")
        .args(["config", "--global", "user.name"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    let has_email = std::process::Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());
    if has_name && has_email {
        report.checks.push(Check::ok(
            PROJECT,
            "git identity",
            "user.name and user.email configured",
        ));
    } else if cfg.container.enabled {
        report.checks.push(Check::warn(
            PROJECT,
            "git identity",
            "user.name or user.email not set in git config",
            "run 'git config --global user.name \"Your Name\"' and 'git config --global user.email you@example.com'",
        ));
    } else {
        report.checks.push(Check::warn(
            PROJECT,
            "git identity",
            "user.name or user.email not set in git config",
            "not required while container.enabled = false",
        ));
    }
}

fn check_tmux(report: &mut Report) {
    match tmux::find_tmux() {
        Some(path) => {
            let detail = if tmux::is_in_tmux() {
                format!("{} (currently inside a session)", path.display())
            } else {
                format!("{} (not currently inside a session)", path.display())
            };
            report.checks.push(Check::ok(TMUX, "tmux", detail));
        }
        None => report.checks.push(Check::warn(
            TMUX,
            "tmux",
            "not found on PATH",
            "'am start' still works — it runs the container directly instead of opening \
             a window. Install tmux for the split-pane workflow.",
        )),
    }
}

fn check_runtime(report: &mut Report, cfg: &Config) -> Option<container::ContainerRuntime> {
    if !cfg.container.enabled {
        report.checks.push(Check::warn(
            RUNTIME,
            "containers",
            "disabled (container.enabled = false)",
            "sessions run directly on the host with no isolation",
        ));
        return None;
    }
    match container::detect_runtime(cfg.container.runtime.clone()) {
        Ok(runtime) => {
            report.checks.push(Check::ok(
                RUNTIME,
                "runtime",
                format!("{} at {}", runtime.kind, runtime.bin.display()),
            ));
            Some(runtime)
        }
        Err(e) => {
            report.checks.push(Check::fail(
                RUNTIME,
                "runtime",
                e.to_string(),
                "install Podman (https://podman.io/docs/installation) or Docker \
                 (https://docs.docker.com/get-docker/), or set container.enabled = false in \
                 .am/config.toml",
            ));
            None
        }
    }
}

/// Where the environment comes from, and whether that source is usable.
/// Report whether a session will be able to authenticate over SSH.
///
/// Worth its own check because the failure is otherwise invisible until a `git push`
/// fails inside a session: mounting `~/.ssh` looks like it should be enough, but a
/// passphrase-protected key cannot be decrypted without a prompt, and keys held only in
/// an agent never appear in `~/.ssh` at all.
fn check_ssh_agent(report: &mut Report, cfg: &Config) {
    if !cfg.container.enabled {
        return;
    }

    if !cfg.container.ssh_agent {
        report.checks.push(Check::ok(
            RUNTIME,
            "ssh agent",
            "not forwarded (container.ssh_agent = false)",
        ));
        return;
    }

    match std::env::var("SSH_AUTH_SOCK").ok().filter(|s| !s.is_empty()) {
        Some(sock) if Path::new(&sock).exists() => report.checks.push(Check::ok(
            RUNTIME,
            "ssh agent",
            format!("forwarding {sock}"),
        )),
        Some(sock) => report.checks.push(Check::warn(
            RUNTIME,
            "ssh agent",
            format!("SSH_AUTH_SOCK points at {sock}, which does not exist"),
            "the agent is not running — start one and `ssh-add` your key, or SSH from \
             inside a session will fall back to the keys in ~/.ssh",
        )),
        None => report.checks.push(Check::warn(
            RUNTIME,
            "ssh agent",
            "no SSH_AUTH_SOCK on the host, so nothing to forward",
            "only an unencrypted key in ~/.ssh will authenticate inside a session; start \
             an agent and `ssh-add` your key to push from one",
        )),
    }
}

fn check_environment(
    report: &mut Report,
    repo_root: &Path,
    cfg: &Config,
    runtime: Option<&container::ContainerRuntime>,
    agent_name: Option<&str>,
) {
    if !cfg.container.enabled {
        return;
    }

    // Discovery runs against the current checkout. A session gets a fresh worktree off
    // HEAD, so an uncommitted config is not what the session will see — worth saying,
    // because it is a genuinely confusing way to lose ten minutes.
    let discovered = match cfg.container.mode {
        ContainerMode::Image => None,
        _ => match devcontainer::find_config(repo_root, cfg.devcontainer.path.as_deref()) {
            Ok(found) => found,
            Err(e) => {
                report.checks.push(Check::fail(
                    ENVIRONMENT,
                    "devcontainer config",
                    e.to_string(),
                    "set devcontainer.path to choose one",
                ));
                return;
            }
        },
    };

    match (&cfg.container.mode, discovered) {
        (ContainerMode::Image, _) => {
            report.checks.push(Check::ok(
                ENVIRONMENT,
                "source",
                "am-managed image (container.mode = \"image\")",
            ));
            check_image_mode(report, cfg, agent_name);
        }
        (ContainerMode::Devcontainer, None) => report.checks.push(Check::fail(
            ENVIRONMENT,
            "source",
            "container.mode is \"devcontainer\" but no devcontainer.json was found",
            "add .devcontainer/devcontainer.json, set devcontainer.path, or use \
             container.mode = \"auto\"",
        )),
        (_, None) => {
            report.checks.push(Check::ok(
                ENVIRONMENT,
                "source",
                "am-managed image (no devcontainer.json found)",
            ));
            check_image_mode(report, cfg, agent_name);
        }
        (_, Some(path)) => {
            report.checks.push(Check::ok(
                ENVIRONMENT,
                "source",
                format!("devcontainer at {}", path.display()),
            ));
            check_devcontainer_mode(report, cfg, runtime, agent_name, &path);
        }
    }
}

fn check_image_mode(report: &mut Report, cfg: &Config, agent_name: Option<&str>) {
    match config::resolve_image(agent_name, cfg) {
        Some(image) => report
            .checks
            .push(Check::ok(ENVIRONMENT, "image", image.to_string())),
        None => report.checks.push(Check::fail(
            ENVIRONMENT,
            "image",
            "no image configured for the selected agent",
            "run 'am setup --agent <name>', or set defaults.agent = \"...\" in \
             .am/config.toml, or set container.image",
        )),
    }
}

fn check_devcontainer_mode(
    report: &mut Report,
    cfg: &Config,
    runtime: Option<&container::ContainerRuntime>,
    agent_name: Option<&str>,
    config_path: &Path,
) {
    // Whether the CLI matters at all depends on the builder *and* on this particular config:
    // under `auto`, a config am can build itself never touches Node, so reporting a missing
    // CLI as a failure would send the user to install something they do not need.
    // Nothing but an explicit `builder = "cli"` reaches the CLI any more: there is no config
    // shape `am` declines and the reference CLI accepts, so `auto` never delegates.
    let needs_cli = cfg.devcontainer.builder == config::Builder::Cli;

    let cli = devcontainer::find_cli(&cfg.devcontainer.cli);
    match (&cli, needs_cli) {
        (Ok(path), _) => {
            let version = probe_version(path, "--version").unwrap_or_else(|| "unknown".to_string());
            report.checks.push(Check::ok(
                ENVIRONMENT,
                "devcontainer CLI",
                format!("{version} at {}", path.display()),
            ));
        }
        (Err(_), true) => report.checks.push(Check::fail(
            ENVIRONMENT,
            "devcontainer CLI",
            format!("'{}' not found on PATH", cfg.devcontainer.cli),
            "npm install -g @devcontainers/cli (needs Node 20+), or set \
             devcontainer.builder = \"auto\" to let am build it",
        )),
        (Err(_), false) => report.checks.push(Check::ok(
            ENVIRONMENT,
            "devcontainer CLI",
            "not needed — am builds devcontainers itself".to_string(),
        )),
    }

    // Node only matters if the CLI is what will run.
    check_node(report, cli.is_ok() || !needs_cli);

    let json = match devcontainer::parse_config(config_path) {
        Ok(json) => json,
        Err(e) => {
            report.checks.push(Check::fail(
                ENVIRONMENT,
                "config",
                e.to_string(),
                "fix the JSON, then re-run 'am doctor'",
            ));
            return;
        }
    };

    if let Err(e) = devcontainer::check_supported(&json) {
        report.checks.push(Check::fail(
            ENVIRONMENT,
            "unsupported",
            first_line(&e.to_string()),
            "set container.mode = \"image\" to use an am-managed image instead",
        ));
    }

    check_gated_constructs(report, cfg, &json);

    // Image currency. The name is derived from the config hash, so "present" and
    // "current" are the same question.
    let injected = injected_features(cfg, agent_name);
    match (devcontainer::image_name(config_path, &injected), runtime) {
        (Ok(image), Some(runtime)) => {
            if devcontainer::image_exists(&runtime.bin, &image) {
                report.checks.push(Check::ok(
                    ENVIRONMENT,
                    "built image",
                    format!("{image} (current)"),
                ));
            } else {
                report.checks.push(Check::warn(
                    ENVIRONMENT,
                    "built image",
                    format!("{image} not built yet"),
                    "the next 'am start' will build it — this can take a few minutes",
                ));
            }
        }
        (Ok(image), None) => report.checks.push(Check::warn(
            ENVIRONMENT,
            "built image",
            format!("{image} (cannot check without a container runtime)"),
            "resolve the container runtime problem above",
        )),
        (Err(e), _) => report.checks.push(Check::fail(
            ENVIRONMENT,
            "built image",
            e.to_string(),
            "check that the config and any referenced Dockerfile are readable",
        )),
    }
}

/// Node is only required to *build*; report accordingly.
fn check_node(report: &mut Report, cli_present: bool) {
    const MIN_MAJOR: u32 = 20;
    match probe_version(Path::new("node"), "--version").and_then(|v| node_major(&v).map(|m| (v, m)))
    {
        Some((version, major)) if major >= MIN_MAJOR => report.checks.push(Check::ok(
            ENVIRONMENT,
            "node",
            format!("{version} (>= {MIN_MAJOR} required)"),
        )),
        Some((version, _)) => report.checks.push(Check::fail(
            ENVIRONMENT,
            "node",
            format!("{version} is too old — the devcontainer CLI needs {MIN_MAJOR} or newer"),
            "upgrade Node, or set container.mode = \"image\"",
        )),
        None if cli_present => report.checks.push(Check::warn(
            ENVIRONMENT,
            "node",
            "not found on PATH",
            "the devcontainer CLI needs Node — if it is bundled with its own runtime this \
             is fine, otherwise install Node 20+",
        )),
        None => report.checks.push(Check::warn(
            ENVIRONMENT,
            "node",
            "not found on PATH",
            "install Node 20+ to build devcontainer images",
        )),
    }
}

/// Constructs `am` will refuse or drop unless explicitly allowed.
fn check_gated_constructs(report: &mut Report, cfg: &Config, json: &devcontainer::DevcontainerJson) {
    if cfg.devcontainer.allow_host_commands {
        // Everything below is permitted; say so once rather than per-construct.
        if json.initialize_command.is_some() || !json.run_args.is_empty() {
            report.checks.push(Check::warn(
                ENVIRONMENT,
                "host commands",
                "allowed (devcontainer.allow_host_commands = true)",
                "initializeCommand runs on your host, outside the container",
            ));
        }
        return;
    }

    if json.initialize_command.is_some() {
        report.checks.push(Check::fail(
            ENVIRONMENT,
            "initializeCommand",
            "present — it runs on your host, so am refuses it",
            "read the command, then set devcontainer.allow_host_commands = true to allow it",
        ));
    }
    if !json.run_args.is_empty() {
        report.checks.push(Check::warn(
            ENVIRONMENT,
            "runArgs",
            format!("{} will be ignored", json.run_args.join(" ")),
            "set devcontainer.allow_host_commands = true to apply them",
        ));
    }
}

/// Mirrors `main::injected_features` so the hash — and therefore the image name — matches
/// what `am start` would compute.
fn injected_features(cfg: &Config, agent_name: Option<&str>) -> Vec<devcontainer::InjectedFeature> {
    let mut features: Vec<devcontainer::InjectedFeature> = cfg
        .devcontainer
        .extra_features
        .iter()
        .map(|(id, options)| devcontainer::InjectedFeature::new(id, options))
        .collect();
    let wants_feature = matches!(
        cfg.devcontainer.agent_install,
        config::AgentInstall::Feature | config::AgentInstall::Auto
    );
    if wants_feature {
        if let Some(feature) = config::resolve_agent_feature(agent_name, cfg) {
            features.push(devcontainer::InjectedFeature::with_defaults(feature));
        }
    }
    features.sort();
    features.dedup();
    features
}

fn check_agent(report: &mut Report, agent_name: Option<&str>) {
    let Some(name) = agent_name else {
        report.checks.push(Check::warn(
            AGENT,
            "agent",
            "none configured",
            "set defaults.agent in config, or pass --agent to 'am start'",
        ));
        return;
    };

    let agent = match container::KnownAgent::parse(name) {
        Ok(agent) => agent,
        Err(e) => {
            report.checks.push(Check::fail(
                AGENT,
                "agent",
                e.to_string(),
                "pick one of: claude, copilot, gemini, codex",
            ));
            return;
        }
    };
    report
        .checks
        .push(Check::ok(AGENT, "agent", agent.to_string()));

    match container::validate_agent_credentials(agent) {
        Ok(()) => report
            .checks
            .push(Check::ok(AGENT, "credentials", "present")),
        Err(e) => report.checks.push(Check::fail(
            AGENT,
            "credentials",
            first_line(&e.to_string()),
            container::credentials_hint(agent),
        )),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run `<bin> <flag>` and return the first line of stdout, or `None` if it cannot run.
fn probe_version(bin: &Path, flag: &str) -> Option<String> {
    let output = std::process::Command::new(bin).arg(flag).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|l| l.trim().to_string())
}

/// Parse the major version out of Node's `v22.23.2`.
fn node_major(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Multi-line errors carry a message and a hint; the report has its own hint column.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Serializes tests that mutate process-wide env vars (HOME, AM_PODMAN_BIN, ...) so
    // they cannot observe each other's state — see CLAUDE.md's Testing section.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn find(report: &Report, name: &str) -> Check {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check named {name:?} in {:?}", names(report)))
            .clone()
    }

    fn names(report: &Report) -> Vec<String> {
        report.checks.iter().map(|c| c.name.clone()).collect()
    }

    fn has(report: &Report, name: &str) -> bool {
        report.checks.iter().any(|c| c.name == name)
    }

    // ── ssh agent ────────────────────────────────────────────────────────────

    fn ssh_agent_report(enabled: bool, sock: Option<&str>) -> Report {
        let mut cfg = Config::default();
        cfg.container.ssh_agent = enabled;
        match sock {
            Some(s) => std::env::set_var("SSH_AUTH_SOCK", s),
            None => std::env::remove_var("SSH_AUTH_SOCK"),
        }
        let mut report = Report::default();
        check_ssh_agent(&mut report, &cfg);
        std::env::remove_var("SSH_AUTH_SOCK");
        report
    }

    #[test]
    fn ssh_agent_reports_a_live_socket_as_ok() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("agent.sock");
        std::fs::write(&sock, "").unwrap();

        let report = ssh_agent_report(true, Some(&sock.to_string_lossy()));

        let check = find(&report, "ssh agent");
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("forwarding"), "{}", check.detail);
    }

    #[test]
    fn ssh_agent_warns_when_the_host_has_no_agent() {
        let report = ssh_agent_report(true, None);

        let check = find(&report, "ssh agent");
        assert_eq!(check.status, Status::Warn);
        assert!(check.hint.unwrap().contains("ssh-add"));
    }

    #[test]
    fn ssh_agent_warns_when_the_socket_is_stale() {
        let tmp = TempDir::new().unwrap();
        let gone = tmp.path().join("gone.sock");

        let report = ssh_agent_report(true, Some(&gone.to_string_lossy()));

        let check = find(&report, "ssh agent");
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("does not exist"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn ssh_agent_opted_out_is_reported_without_a_warning() {
        // Off is a choice, not a problem — an agent on the host must not turn it into one.
        let report = ssh_agent_report(false, Some("/run/user/1000/keyring/ssh"));

        let check = find(&report, "ssh agent");
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("not forwarded"), "{}", check.detail);
    }

    // ── Color ────────────────────────────────────────────────────────────────

    #[test]
    fn render_colors_each_status_differently() {
        let report = Report {
            checks: vec![
                Check::ok("S", "fine", "detail"),
                Check::warn("S", "note", "detail", "hint"),
                Check::fail("S", "broken", "detail", "hint"),
            ],
        };
        let out = report.render(true);

        assert!(out.contains("\x1b[32m✓\x1b[0m"), "ok should be green");
        assert!(out.contains("\x1b[33m!\x1b[0m"), "warn should be yellow");
        assert!(out.contains("\x1b[31m✗\x1b[0m"), "fail should be red");
    }

    #[test]
    fn hints_are_dimmed_rather_than_colored_by_severity() {
        let report = Report {
            checks: vec![Check::fail("S", "broken", "detail", "do this")],
        };
        let out = report.render(true);

        // The finding is red, the hint attached to it is dim — a second red line would
        // read as a second problem.
        assert!(out.contains("\x1b[31m✗\x1b[0m"));
        assert!(out.contains("\x1b[2m      → do this\x1b[0m"));
    }

    #[test]
    fn render_without_color_has_no_escape_sequences() {
        let report = Report {
            checks: vec![
                Check::ok("S", "fine", "detail"),
                Check::fail("S", "broken", "detail", "hint"),
            ],
        };
        let out = report.render(false);

        // Piped output and NO_COLOR users get this form; a stray escape here would end
        // up in log files and in the cucumber assertions.
        assert!(!out.contains('\x1b'), "unexpected escape in: {out:?}");
    }

    /// Remove every ANSI SGR sequence. Written generically rather than as a list of the
    /// codes in use, so adding a style cannot quietly stop the alignment test checking
    /// anything.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn color_does_not_disturb_column_alignment() {
        let report = Report {
            checks: vec![
                Check::ok("S", "short", "detail"),
                Check::fail("S", "short", "detail", "hint"),
            ],
        };
        // Every painted run is one visible column wide either way, so stripping the
        // escapes has to reproduce the plain render byte for byte.
        assert_eq!(strip_ansi(&report.render(true)), report.render(false));
    }

    #[test]
    fn the_verdict_line_takes_the_worst_severity() {
        let ready = Report {
            checks: vec![Check::ok("S", "a", "d")],
        };
        assert!(ready.render(true).contains("\x1b[32mReady."));

        let noted = Report {
            checks: vec![Check::warn("S", "a", "d", "h")],
        };
        assert!(noted.render(true).contains("\x1b[33mReady. Some notes"));

        let broken = Report {
            checks: vec![Check::fail("S", "a", "d", "h")],
        };
        assert!(broken.render(true).contains("\x1b[31m1 problem"));
    }

    // ── Config loading ────────────────────────────────────────────────────────

    #[test]
    fn config_check_reports_a_broken_project_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        std::fs::write(
            tmp.path().join(".am").join("config.toml"),
            "[tmux]\nsplit_percent = 500\n",
        )
        .unwrap();

        let mut report = Report::default();
        check_config(&mut report, Some(tmp.path()));

        let check = find(&report, "config");
        assert_eq!(check.status, Status::Fail);
        assert!(
            check.hint.is_some(),
            "a failing config check must say what to do about it"
        );
    }

    #[test]
    fn shadowed_config_in_a_worktree_is_reported() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path();
        let worktree = repo_root.join(".am").join("worktrees").join("feat");
        std::fs::create_dir_all(worktree.join(".am")).unwrap();
        let shadowed = worktree.join(".am").join("config.toml");
        std::fs::write(&shadowed, "[defaults]\nagent = \"claude\"\n").unwrap();

        let mut report = Report::default();
        check_shadowed_config(&mut report, &worktree, repo_root);

        let check = find(&report, "shadowed config");
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains(&shadowed.display().to_string()));
        // The hint has to name the file that *is* read, or the user is left guessing.
        assert!(check
            .hint
            .unwrap()
            .contains(&repo_root.join(".am").join("config.toml").display().to_string()));
    }

    #[test]
    fn no_shadowed_config_warning_at_the_repo_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        std::fs::write(
            tmp.path().join(".am").join("config.toml"),
            "[defaults]\nagent = \"claude\"\n",
        )
        .unwrap();

        let mut report = Report::default();
        check_shadowed_config(&mut report, tmp.path(), tmp.path());

        assert!(!has(&report, "shadowed config"));
    }

    #[test]
    fn no_shadowed_config_warning_for_an_ordinary_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        let mut report = Report::default();
        check_shadowed_config(&mut report, &sub, tmp.path());

        assert!(!has(&report, "shadowed config"));
    }

    #[test]
    fn config_check_names_the_files_it_read() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        let project = tmp.path().join(".am").join("config.toml");
        std::fs::write(&project, "[defaults]\nagent = \"claude\"\n").unwrap();

        let mut report = Report::default();
        check_config(&mut report, Some(tmp.path()));

        let detail = find(&report, "config").detail;
        assert!(
            detail.contains(&project.display().to_string()),
            "the loaded project config must be named, got: {detail}"
        );
    }

    #[test]
    fn config_check_says_so_when_there_is_no_project_config() {
        let tmp = TempDir::new().unwrap();

        let mut report = Report::default();
        check_config(&mut report, Some(tmp.path()));

        let detail = find(&report, "config").detail;
        assert!(
            detail.contains("no project config at"),
            "an absent project config is itself the answer, got: {detail}"
        );
    }

    #[test]
    fn config_check_warns_about_unrecognised_keys() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        std::fs::write(
            tmp.path().join(".am").join("config.toml"),
            "[defaults]\nagnet = \"claude\"\n",
        )
        .unwrap();

        let mut report = Report::default();
        check_config(&mut report, Some(tmp.path()));

        let check = find(&report, "config");
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.contains("defaults.agnet"),
            "the offending key must be named, got: {}",
            check.detail
        );
    }

    #[test]
    fn config_check_passes_on_a_valid_project_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        std::fs::write(
            tmp.path().join(".am").join("config.toml"),
            "[defaults]\nagent = \"claude\"\n",
        )
        .unwrap();

        let mut report = Report::default();
        let cfg = check_config(&mut report, Some(tmp.path()));

        assert_eq!(find(&report, "config").status, Status::Ok);
        assert_eq!(cfg.agent.as_deref(), Some("claude"));
    }

    // ── Report shape ──────────────────────────────────────────────────────────

    #[test]
    fn failures_and_warnings_are_counted_separately() {
        let report = Report {
            checks: vec![
                Check::ok("S", "a", "fine"),
                Check::warn("S", "b", "hmm", "do this"),
                Check::fail("S", "c", "broken", "fix this"),
                Check::fail("S", "d", "broken", "fix this"),
            ],
        };
        assert_eq!(report.failures(), 2);
        assert_eq!(report.warnings(), 1);
    }

    #[test]
    fn render_groups_checks_under_their_section() {
        let report = Report {
            checks: vec![
                Check::ok("First", "a", "detail-a"),
                Check::ok("First", "b", "detail-b"),
                Check::ok("Second", "c", "detail-c"),
            ],
        };
        let out = report.render(false);
        assert_eq!(out.matches("First").count(), 1, "section repeated: {out}");
        assert!(out.find("First").unwrap() < out.find("Second").unwrap());
    }

    #[test]
    fn render_includes_hints_only_where_present() {
        let report = Report {
            checks: vec![
                Check::ok("S", "fine", "all good"),
                Check::fail("S", "broken", "nope", "run 'am init'"),
            ],
        };
        let out = report.render(false);
        assert!(out.contains("→ run 'am init'"));
        assert_eq!(out.matches('→').count(), 1);
    }

    #[test]
    fn render_summarises_readiness() {
        let ready = Report {
            checks: vec![Check::ok("S", "a", "fine")],
        };
        assert!(ready.render(false).ends_with("Ready.\n"));

        let noted = Report {
            checks: vec![Check::warn("S", "a", "hmm", "note")],
        };
        assert!(noted.render(false).contains("worth reading"));

        let broken = Report {
            checks: vec![Check::fail("S", "a", "no", "fix")],
        };
        assert!(broken.render(false).contains("prevent 'am start'"));
    }

    // ── Repository ────────────────────────────────────────────────────────────

    #[test]
    fn missing_repository_is_a_failure_not_an_error() {
        // Being outside a repo is one of the things doctor exists to tell you.
        let report = run(None, None);
        let check = find(&report, "version control");
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("not inside a git or jj repository"));
    }

    #[test]
    fn outside_a_repo_skips_the_repo_specific_checks() {
        let report = run(None, None);
        assert!(!has(&report, ".am/"));
        assert!(!has(&report, "source"));
    }

    #[test]
    fn a_git_repo_is_reported_with_its_path() {
        let tmp = TempDir::new().unwrap();
        let report = run(Some((tmp.path(), Vcs::Git)), None);
        let check = find(&report, "version control");
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.starts_with("git repository at"));
    }

    #[test]
    fn a_jj_repo_is_reported_as_jj() {
        let tmp = TempDir::new().unwrap();
        let report = run(Some((tmp.path(), Vcs::Jj)), None);
        assert!(find(&report, "version control").detail.starts_with("jj"));
    }

    // ── Project setup ─────────────────────────────────────────────────────────

    #[test]
    fn uninitialised_project_points_at_am_init() {
        let tmp = TempDir::new().unwrap();
        let report = run(Some((tmp.path(), Vcs::Git)), None);
        let check = find(&report, ".am/");
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.hint.as_deref(), Some("run 'am init'"));
    }

    #[test]
    fn initialised_project_reports_ok() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".am")).unwrap();
        let report = run(Some((tmp.path(), Vcs::Git)), None);
        assert_eq!(find(&report, ".am/").status, Status::Ok);
        // git identity check depends on the test runner's actual git config,
        // so we only verify the check exists, not its status.
        find(&report, "git identity");
    }

    // ── Agent ─────────────────────────────────────────────────────────────────

    #[test]
    fn unknown_agent_is_rejected_with_the_valid_names() {
        let tmp = TempDir::new().unwrap();
        let report = run(Some((tmp.path(), Vcs::Git)), Some("not-an-agent"));
        let check = find(&report, "agent");
        assert_eq!(check.status, Status::Fail);
        assert!(check.hint.as_deref().unwrap().contains("claude"));
    }

    #[test]
    fn a_known_agent_is_reported_by_name() {
        let tmp = TempDir::new().unwrap();
        let report = run(Some((tmp.path(), Vcs::Git)), Some("claude"));
        let check = find(&report, "agent");
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "claude");
    }

    #[test]
    fn missing_credentials_hint_matches_container_credentials_hint() {
        // HOME points somewhere with no ~/.claude, so credentials is guaranteed to fail
        // regardless of whatever the developer running the suite has signed into.
        let _g = lock_env();
        let tmp = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");

        let report = run(Some((tmp.path(), Vcs::Git)), Some("claude"));

        std::env::remove_var("HOME");

        let check = find(&report, "credentials");
        assert_eq!(check.status, Status::Fail);
        assert_eq!(
            check.hint.as_deref(),
            Some(container::credentials_hint(container::KnownAgent::Claude))
        );
    }

    #[test]
    fn missing_runtime_hint_names_both_install_links() {
        let _g = lock_env();
        std::env::set_var("AM_PODMAN_BIN", "/does/not/exist/podman");
        std::env::set_var("AM_DOCKER_BIN", "/does/not/exist/docker");

        let mut report = Report::default();
        let mut cfg = Config::default();
        cfg.container.enabled = true;
        check_runtime(&mut report, &cfg);

        std::env::remove_var("AM_PODMAN_BIN");
        std::env::remove_var("AM_DOCKER_BIN");

        let check = find(&report, "runtime");
        assert_eq!(check.status, Status::Fail);
        let hint = check.hint.unwrap();
        assert!(hint.contains("https://podman.io/docs/installation"), "{hint}");
        assert!(hint.contains("https://docs.docker.com/get-docker/"), "{hint}");
        assert!(hint.contains("container.enabled = false"), "{hint}");
    }

    #[test]
    fn missing_image_hint_names_a_concrete_fix() {
        let mut report = Report::default();
        let cfg = Config::default();
        check_image_mode(&mut report, &cfg, None);

        let check = find(&report, "image");
        assert_eq!(check.status, Status::Fail);
        let hint = check.hint.unwrap();
        assert!(hint.contains("am setup --agent"), "{hint}");
        assert!(hint.contains("defaults.agent"), "{hint}");
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    #[test]
    fn node_major_parses_the_v_prefixed_form() {
        assert_eq!(node_major("v22.23.2"), Some(22));
        assert_eq!(node_major("20.1.0"), Some(20));
        assert_eq!(node_major("not a version"), None);
    }

    #[test]
    fn first_line_drops_the_hint_half_of_an_error() {
        assert_eq!(first_line("something failed\nHint: try this"), "something failed");
        assert_eq!(first_line("single line"), "single line");
    }
}
