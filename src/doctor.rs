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
    pub fn render(&self) -> String {
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
                check.status.glyph(),
                check.name,
                check.detail
            ));
            if let Some(ref hint) = check.hint {
                // Indented under the check it belongs to, so a wall of hints cannot be
                // mistaken for a list of separate problems.
                out.push_str(&format!("      → {hint}\n"));
            }
        }

        let (failures, warnings) = (self.failures(), self.warnings());
        out.push('\n');
        if failures > 0 {
            let noun = if failures == 1 { "problem" } else { "problems" };
            out.push_str(&format!(
                "{failures} {noun} will prevent 'am start' from working.\n"
            ));
        } else if warnings > 0 {
            out.push_str("Ready. Some notes above are worth reading.\n");
        } else {
            out.push_str("Ready.\n");
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
    let cfg = load_config(repo_root);

    if let Some(root) = repo_root {
        check_project_setup(&mut report, root, &cfg);
    }
    check_tmux(&mut report);

    let runtime = check_runtime(&mut report, &cfg);
    let agent_name = effective_agent(agent_flag, &cfg);

    if let Some(root) = repo_root {
        check_environment(&mut report, root, &cfg, runtime.as_ref(), agent_name.as_deref());
    }
    check_agent(&mut report, agent_name.as_deref());

    report
}

/// Load config the same way `cmd_start` does, falling back to global-only outside a repo.
fn load_config(repo_root: Option<&Path>) -> Config {
    let project = repo_root.map(|r| r.join(".am").join("config.toml"));
    config::load_with_global(config::global_config_path().as_deref(), project.as_deref())
        .unwrap_or_default()
}

fn effective_agent(agent_flag: Option<&str>, cfg: &Config) -> Option<String> {
    agent_flag.map(str::to_string).or_else(|| cfg.agent.clone())
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
                "install Podman or Docker, or set container.enabled = false",
            ));
            None
        }
    }
}

/// Where the environment comes from, and whether that source is usable.
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
            "set an agent with --agent or defaults.agent, or set container.image",
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
    // The CLI is only needed to *build*. An already-built image means a session can start
    // without it, so a missing CLI is not automatically fatal.
    let cli = devcontainer::find_cli(&cfg.devcontainer.cli);
    match &cli {
        Ok(path) => {
            let version = probe_version(path, "--version").unwrap_or_else(|| "unknown".to_string());
            report.checks.push(Check::ok(
                ENVIRONMENT,
                "devcontainer CLI",
                format!("{version} at {}", path.display()),
            ));
        }
        Err(_) => report.checks.push(Check::fail(
            ENVIRONMENT,
            "devcontainer CLI",
            format!("'{}' not found on PATH", cfg.devcontainer.cli),
            "npm install -g @devcontainers/cli (needs Node 20+), or set \
             container.mode = \"image\"",
        )),
    }

    check_node(report, cli.is_ok());

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
            format!("authenticate {agent} on this machine, then re-run 'am doctor'"),
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
        let out = report.render();
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
        let out = report.render();
        assert!(out.contains("→ run 'am init'"));
        assert_eq!(out.matches('→').count(), 1);
    }

    #[test]
    fn render_summarises_readiness() {
        let ready = Report {
            checks: vec![Check::ok("S", "a", "fine")],
        };
        assert!(ready.render().ends_with("Ready.\n"));

        let noted = Report {
            checks: vec![Check::warn("S", "a", "hmm", "note")],
        };
        assert!(noted.render().contains("worth reading"));

        let broken = Report {
            checks: vec![Check::fail("S", "a", "no", "fix")],
        };
        assert!(broken.render().contains("prevent 'am start'"));
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
