mod cli;
mod command;
mod config;
mod container;
mod devcontainer;
mod doctor;
mod error;
mod messages;
mod session;
mod tmux;
mod worktree;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Parser;
use cli::{Cli, Commands, SessionCommands};
use command::shell_quote;

/// Build a stable, globally unique container name from a repo and session slug.
fn container_name(repo_root: &Path, slug: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(repo_root.as_os_str().as_encoded_bytes());
    let prefix: String = hash.iter().take(3).map(|b| format!("{b:02x}")).collect();
    format!("am-{slug}-{prefix}")
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Choose context-appropriate wording once, up front, based on tmux state.
    let msgs = messages::Messages::from_env();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Start {
            slug,
            agent,
            no_container,
            auto,
            rebuild,
        } => cmd_start(&slug, agent.as_deref(), no_container, auto, rebuild),
        Commands::List { all } => cmd_list(all),
        Commands::Attach { slug } => cmd_attach(&slug),
        Commands::Run { slug, agent } => cmd_run(&slug, &agent),
        Commands::Destroy { slug, force } => cmd_destroy(&slug, force, &msgs),
        Commands::Doctor => cmd_doctor(None),
        Commands::GenerateConfig => cmd_generate_config(),
        Commands::Session { command } => match command {
            SessionCommands::Rm { slug, repo, force } => {
                cmd_session_rm(&slug, repo.as_deref(), force)
            }
        },
    }
}

// ── am init ───────────────────────────────────────────────────────────────────

fn cmd_init() -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;

    let am_dir = repo_root.join(".am");
    std::fs::create_dir_all(&am_dir)?;

    let config_path = am_dir.join("config.toml");
    if !config_path.exists() {
        config::write_defaults(&config_path)?;
        println!("Created .am/config.toml");
    } else {
        println!(".am/config.toml already exists, skipping");
    }

    let gitignore_path = repo_root.join(".gitignore");
    let gitignore_content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    // Check for old-style broad .am/ entry and print advisory.
    let has_old_am_entry = gitignore_content
        .lines()
        .any(|l| l.trim() == ".am/" || l.trim() == ".am");
    if has_old_am_entry {
        println!(
            "Note: .am/ is in .gitignore; .am/config.toml is now committable — \
             you may want to narrow this to .am/worktrees/"
        );
    }

    // Check if .am/worktrees/ is already present.
    let worktrees_already_ignored = gitignore_content
        .lines()
        .any(|l| l.trim() == ".am/worktrees/" || l.trim() == ".am/worktrees");
    if worktrees_already_ignored {
        println!(".am/worktrees/ already in .gitignore, skipping");
    } else {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore_path)?;
        if !gitignore_content.is_empty() && !gitignore_content.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        file.write_all(b".am/worktrees/\n")?;
        println!("Added .am/worktrees/ to .gitignore");
    }

    println!("am initialized. Run 'am start <slug>' to create your first session.");
    Ok(())
}

// ── am start ──────────────────────────────────────────────────────────────────

fn cmd_start(
    slug: &str,
    agent_flag: Option<&str>,
    no_container: bool,
    auto: bool,
    rebuild: bool,
) -> anyhow::Result<()> {
    let (repo_root, vcs) = find_repo_root()?;

    // Migration: transparently move old per-repo sessions to the global store.
    // Ignore migration errors — they are warnings, not blockers.
    if let Err(e) = session::migrate_sessions(&repo_root) {
        eprintln!("warning: session migration failed: {e}");
    }

    let sessions = session::load_sessions_for_repo(&repo_root)?;

    if sessions.iter().any(|s| s.slug == slug) {
        return Err(error::AmError::SlugAlreadyExists(slug.to_string()).into());
    }

    let window_name = format!("am-{slug}");
    let container_name = container_name(&repo_root, slug);

    // Load config
    let project_config_path = repo_root.join(".am").join("config.toml");
    let cfg = config::load_with_global(
        config::global_config_path().as_deref(),
        Some(&project_config_path),
    )?;

    // Effective agent: --agent flag > config.container.agent > config.agent
    let effective_agent = agent_flag
        .map(str::to_string)
        .or_else(|| cfg.container.agent.clone())
        .or_else(|| cfg.agent.clone());

    // ── Early validation (fail before any side effects) ───────────────────────

    // 1. VCS (already resolved by find_repo_root)

    // 2. Auto mode constraints
    if auto && no_container {
        return Err(error::AmError::AutoRequiresContainer.into());
    }
    if auto && effective_agent.is_none() {
        return Err(error::AmError::AutoRequiresAgent.into());
    }

    // 3. Parse agent name — validates and gives a typed KnownAgent for container functions
    let effective_known_agent: Option<container::KnownAgent> = effective_agent
        .as_deref()
        .map(container::KnownAgent::parse)
        .transpose()?;

    // 4. Generate gitconfig in the global state directory.
    //    This replaces the old .am/gitconfig check — we generate it unconditionally.
    let state_dir = config::global_state_dir()
        .ok_or(error::AmError::GlobalStateDirNotFound)?;
    std::fs::create_dir_all(&state_dir)?;
    let name = read_git_config("user.name").unwrap_or_default();
    let email = read_git_config("user.email").unwrap_or_default();
    let gitconfig_path = state_dir.join("gitconfig");
    std::fs::write(&gitconfig_path, format!("[user]\n\tname = {name}\n\temail = {email}\n"))?;

    // 5. Container runtime and host-side credentials. Everything checkable without a
    //    worktree is checked here; a devcontainer config only exists once the worktree
    //    does, so its half of preflight happens below.
    let use_container = cfg.container.enabled && !no_container;
    let runtime = if use_container {
        let runtime = container::detect_runtime(cfg.container.runtime.clone())?;
        if let Some(agent) = effective_known_agent {
            container::validate_agent_credentials(agent)?;
        }
        // Pre-emptively remove any leftover container from a previous run
        container::remove_if_exists(&runtime, &container_name);
        Some(runtime)
    } else {
        None
    };

    // ── Worktree ──────────────────────────────────────────────────────────────
    //
    // Guarded from here on: a failed build, a declined trust prompt, or a tmux error
    // must not leave an orphaned worktree that `am destroy` cannot see.
    let guard = worktree::WorktreeGuard::create(slug, &repo_root, vcs.clone())?;
    let worktree_path = guard.path().to_path_buf();

    // ── Post-worktree preflight: the devcontainer config, if any ──────────────
    let (container_cmd, mut session_container) = match runtime {
        Some(ref runtime) => {
            let plan = plan_container(ContainerPlanInput {
                slug,
                repo_root: &repo_root,
                worktree: &worktree_path,
                vcs: &vcs,
                cfg: &cfg,
                runtime,
                agent: effective_known_agent,
                agent_name: effective_agent.as_deref(),
                auto,
                rebuild,
                container_name: &container_name,
            })?;
            (Some(plan.cmd), Some(plan.session))
        }
        None => (None, None),
    };
    if let Some(container) = &mut session_container {
        container.container_name = Some(container_name.clone());
    }
    // The -p percent always describes the new pane, which is always the agent's.
    let (agent_pane_idx, shell_pane_idx, split_before) = pane_layout(&cfg.tmux.agent_pane);

    let tmux_window_id = if tmux::is_in_tmux() {
        // Create a dedicated window for the session instead of commandeering the
        // caller's current window. `new-window` opens both panes in the worktree
        // and switches focus to the new window automatically.
        let window_id = tmux::create_window(&window_name, &worktree_path)
            .map_err(|e| anyhow::anyhow!(
                "{e}\nHint: a window named '{window_name}' may already exist — run 'am destroy {slug}' first"
            ))?;
        let container_shell_cmd = container_cmd.as_ref().map(|cmd| {
            cmd.iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" ")
        });
        tmux::split_window(
            &window_id,
            &worktree_path,
            &cfg.tmux.split,
            cfg.tmux.split_percent,
            split_before,
            container_shell_cmd.as_deref(),
        )
        .with_context(|| {
            format!("tmux split failed — run 'am destroy --force {slug}' to clean up")
        })?;
        // A container command was exec'd by the split above. A host-side agent is short
        // enough to type, and typing it leaves a shell behind when the agent exits.
        if container_shell_cmd.is_none() {
            if let Some(ref agent) = effective_agent {
                tmux::send_keys(&tmux::get_pane_id(&window_id, agent_pane_idx), agent)?;
            }
        }
        // Both panes already open in the worktree (via `new-window -c`), so no cd
        // is needed. Keep focus on the shell pane.
        tmux::select_pane(&tmux::get_pane_id(&window_id, shell_pane_idx))?;
        Some(window_id)
    } else if container_cmd.is_none() {
        println!("Note: not inside tmux — no window opened. Run 'am attach {slug}' from inside tmux to open one.");
        None
    } else {
        None
    };

    let new_session = session::Session {
        slug: slug.to_string(),
        created_at: chrono::Utc::now(),
        auto,
        repo_root: repo_root.clone(),
        vcs: session::VcsMetadata {
            branch: format!("am/{slug}"),
            worktree_path: worktree_path.clone(),
        },
        tmux: session::TmuxMetadata {
            tmux_window: window_name.clone(),
            tmux_window_id: tmux_window_id.clone(),
            agent_pane: tmux::get_pane_id(tmux_window_id.as_deref().unwrap_or(&window_name), agent_pane_idx),
            shell_pane: tmux::get_pane_id(tmux_window_id.as_deref().unwrap_or(&window_name), shell_pane_idx),
            // Sessions now own a dedicated window; there is no prior window state
            // to restore on destroy. These remain for compatibility with records
            // written by older versions of `am`.
            original_window_name: None,
            original_shell_dir: None,
        },
        container: session_container,
    };

    // Not in tmux with a container: record the session then replace this process.
    // Recording before exec ensures the session is always tracked; if exec fails
    // the user can run 'am destroy <slug>' to clean up.
    if let Some(ref cmd) = container_cmd {
        if !tmux::is_in_tmux() {
            session::add_session_global(new_session)?;
            guard.commit();
            println!("Started session '{slug}'");
            println!("  worktree:  {}", worktree_path.display());
            println!("  container: {container_name}");
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
                // exec() only returns on failure
                return Err(error::AmError::ContainerError(format!(
                    "failed to exec container: {err} — run 'am destroy --force {slug}' to clean up"
                ))
                .into());
            }
            #[cfg(not(unix))]
            {
                let status = std::process::Command::new(&cmd[0])
                    .args(&cmd[1..])
                    .status()
                    .map_err(|e| {
                        error::AmError::ContainerError(format!("failed to run container: {e}"))
                    })?;
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }

    let mode = new_session
        .container
        .as_ref()
        .map(|c| (c.mode, c.image.clone()));
    session::add_session_global(new_session)?;
    guard.commit();

    println!("Started session '{slug}'");
    println!("  worktree:  {}", worktree_path.display());
    println!("  branch:    am/{slug}");
    if let Some((mode, image)) = mode {
        println!("  container: {container_name}");
        if mode == session::ContainerMode::Devcontainer {
            println!("  image:     {image} (from devcontainer.json)");
        }
    }
    Ok(())
}

// ── am start: container planning ──────────────────────────────────────────────

/// Inputs for resolving the container half of a session. Grouped into a struct because
/// these all travel together and a positional argument list this long invites mix-ups.
struct ContainerPlanInput<'a> {
    slug: &'a str,
    repo_root: &'a std::path::Path,
    worktree: &'a std::path::Path,
    vcs: &'a config::Vcs,
    cfg: &'a config::Config,
    runtime: &'a container::ContainerRuntime,
    agent: Option<container::KnownAgent>,
    agent_name: Option<&'a str>,
    auto: bool,
    rebuild: bool,
    container_name: &'a str,
}

struct ContainerPlan {
    cmd: Vec<String>,
    session: session::SessionContainer,
}

/// Decide whether this session runs from an `am`-resolved image or the repo's own
/// devcontainer, and assemble the run command either way.
fn plan_container(input: ContainerPlanInput) -> anyhow::Result<ContainerPlan> {
    let ContainerPlanInput {
        slug,
        repo_root,
        worktree,
        vcs,
        cfg,
        runtime,
        agent,
        agent_name,
        auto,
        rebuild,
        container_name,
    } = input;

    // Discovery is relative to the worktree: the config is a checked-in, branch-specific
    // file, so two sessions on different branches may legitimately disagree about it.
    let discovered = match cfg.container.mode {
        config::ContainerMode::Image => None,
        _ => devcontainer::find_config(worktree, cfg.devcontainer.path.as_deref())?,
    };

    let config_path = match (&cfg.container.mode, discovered) {
        (config::ContainerMode::Image, _) => None,
        (config::ContainerMode::Devcontainer, None) => {
            return Err(anyhow::anyhow!(
                "container.mode is \"devcontainer\" but no devcontainer.json was found in {}\n\
                 Add .devcontainer/devcontainer.json, set devcontainer.path, or use \
                 container.mode = \"auto\"",
                worktree.display()
            ))
        }
        (_, found) => found,
    };

    match config_path {
        Some(path) => plan_devcontainer(
            slug, repo_root, worktree, vcs, cfg, runtime, agent, agent_name, auto, rebuild, &path,
            container_name,
        ),
        None => plan_image(slug, repo_root, vcs, cfg, runtime, agent, agent_name, auto,
            container_name),
    }
}

/// The pre-existing path: an image `am` resolves from config.
#[allow(clippy::too_many_arguments)]
fn plan_image(
    slug: &str,
    repo_root: &std::path::Path,
    vcs: &config::Vcs,
    cfg: &config::Config,
    runtime: &container::ContainerRuntime,
    agent: Option<container::KnownAgent>,
    agent_name: Option<&str>,
    auto: bool,
    container_name: &str,
) -> anyhow::Result<ContainerPlan> {
    let image = config::resolve_image(agent_name, cfg)
        .ok_or(error::AmError::ContainerImageNotConfigured)?;
    let home = container::container_home(&cfg.container.user, cfg.devcontainer.home.as_deref());
    let agent_auth = match agent {
        Some(agent) => container::preflight_agent_auth(agent, &home)?,
        None => container::AgentAuth::default(),
    };
    let mounts = container::resolve_mounts(
        slug,
        repo_root,
        vcs,
        agent_auth.mounts.clone(),
        cfg.container.gitconfig.as_deref(),
        cfg.container.ssh.as_deref(),
        &cfg.container.user,
        cfg.devcontainer.home.as_deref(),
    )?;
    let mut cmd = container::build_run_command(
        runtime,
        image,
        &mounts,
        &cfg.container.env,
        &agent_auth.env,
        &cfg.container.network,
        container_name,
        &container::DevcontainerRuntime::default(),
    );
    cmd.extend(agent_command(agent, auto));
    Ok(ContainerPlan {
        cmd,
        session: session::SessionContainer::image_mode(
            runtime.kind.to_string(),
            image.to_string(),
        ),
    })
}

/// The devcontainer path: build the repo's own config into an image, then run it with
/// `am`'s mounts, user mapping, and network policy.
#[allow(clippy::too_many_arguments)]
fn plan_devcontainer(
    slug: &str,
    repo_root: &std::path::Path,
    worktree: &std::path::Path,
    vcs: &config::Vcs,
    cfg: &config::Config,
    runtime: &container::ContainerRuntime,
    agent: Option<container::KnownAgent>,
    agent_name: Option<&str>,
    auto: bool,
    rebuild: bool,
    config_path: &std::path::Path,
    container_name: &str,
) -> anyhow::Result<ContainerPlan> {
    let json = devcontainer::parse_config(config_path)?;
    devcontainer::check_supported(&json)?;
    devcontainer::check_host_commands(&json, cfg.devcontainer.allow_host_commands)?;

    let injected = injected_features(cfg, agent_name);
    let image = devcontainer::image_name(config_path, &injected)?;
    let config_hash = devcontainer::config_hash(config_path, &injected)?;

    // The whole point of hashing: an unchanged config reuses its image, so the Node CLI
    // runs once per config change rather than once per session.
    if rebuild || !devcontainer::image_exists(&runtime.bin, &image) {
        let cli = devcontainer::find_cli(&cfg.devcontainer.cli)?;
        println!("Building devcontainer image {image} (this can take a few minutes)...");
        devcontainer::build(
            &cli,
            worktree,
            config_path,
            &image,
            &injected,
            &runtime.bin,
            rebuild,
        )?;
    }

    let snippets = devcontainer::read_image_metadata(&runtime.bin, &image)?;
    let ctx = devcontainer::SubstitutionContext::new(
        worktree,
        &json
            .workspace_folder
            .clone()
            .unwrap_or_else(|| worktree.to_string_lossy().into_owned()),
    );
    let resolved = devcontainer::finalize(devcontainer::merge(&snippets)?, &json, &ctx);

    // remoteUser decides where credentials must land — an image running as root keeps its
    // config in /root, not /home/root.
    let user = resolved
        .remote_user
        .clone()
        .or_else(|| resolved.container_user.clone())
        .unwrap_or_else(|| cfg.container.user.clone());
    let home = container::container_home(&user, cfg.devcontainer.home.as_deref());

    let trusted = devcontainer::apply_trust(&resolved, cfg);
    let agent_auth = match agent {
        Some(agent) => container::preflight_agent_auth(agent, &home)?,
        None => container::AgentAuth::default(),
    };
    let mut mounts = container::resolve_mounts(
        slug,
        repo_root,
        vcs,
        agent_auth.mounts.clone(),
        cfg.container.gitconfig.as_deref(),
        cfg.container.ssh.as_deref(),
        &user,
        cfg.devcontainer.home.as_deref(),
    )?;
    mounts.container_home = home;

    let mut cmd = container::build_run_command(
        runtime,
        &image,
        &mounts,
        &cfg.container.env,
        &agent_auth.env,
        &cfg.container.network,
        container_name,
        &trusted,
    );
    // Feature entrypoints and lifecycle hooks must run before the agent, and am overrides
    // the image's own ENTRYPOINT to launch it — so they have to be chained explicitly.
    let (hooks, hooks_ran) =
        devcontainer::startup_commands(&resolved, cfg.devcontainer.skip_lifecycle);
    let mut chain = trusted.entrypoints.clone();
    chain.extend(hooks);
    cmd.extend(container::compose_entrypoint_command(
        &chain,
        &agent_command(agent, auto),
    ));

    Ok(ContainerPlan {
        cmd,
        session: session::SessionContainer {
            runtime: runtime.kind.to_string(),
            image,
            container_name: None,
            container_id: None,
            mode: session::ContainerMode::Devcontainer,
            config_path: Some(config_path.to_path_buf()),
            config_hash: Some(config_hash),
            remote_user: resolved.remote_user.clone(),
            lifecycle_done: hooks_ran,
        },
    })
}

/// The agent invocation appended as the container command, if there is one.
fn agent_command(agent: Option<container::KnownAgent>, auto: bool) -> Vec<String> {
    let Some(agent) = agent else {
        return Vec::new();
    };
    let mut cmd = vec![agent.to_string()];
    if auto {
        cmd.extend(container::agent_auto_flags(agent));
    }
    cmd
}

/// Features `am` injects at build time: the agent's own, plus anything configured.
fn injected_features(
    cfg: &config::Config,
    agent_name: Option<&str>,
) -> Vec<devcontainer::InjectedFeature> {
    let mut features: Vec<devcontainer::InjectedFeature> = cfg
        .devcontainer
        .extra_features
        .iter()
        .map(|(id, options)| devcontainer::InjectedFeature::new(id, options))
        .collect();

    let wants_feature = match cfg.devcontainer.agent_install {
        config::AgentInstall::Feature | config::AgentInstall::Auto => true,
        config::AgentInstall::Bootstrap | config::AgentInstall::None => false,
    };
    if wants_feature {
        if let Some(feature) = config::resolve_agent_feature(agent_name, cfg) {
            features.push(devcontainer::InjectedFeature::with_defaults(feature));
        }
    }
    features.sort();
    features.dedup();
    features
}

// ── am list ───────────────────────────────────────────────────────────────────

fn cmd_list(all: bool) -> anyhow::Result<()> {
    if all {
        cmd_list_all()
    } else {
        cmd_list_repo()
    }
}

fn cmd_list_repo() -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root().map_err(|e| {
        anyhow::anyhow!("{e}\nRun 'am list --all' to see sessions from all repos.")
    })?;

    // Migration: transparently move old per-repo sessions to the global store.
    if let Err(e) = session::migrate_sessions(&repo_root) {
        eprintln!("warning: session migration failed: {e}");
    }

    let sessions = session::load_sessions_for_repo(&repo_root)?;

    if sessions.is_empty() {
        println!("No active sessions. Run 'am start <slug>' to begin.");
        return Ok(());
    }

    let slug_w = sessions
        .iter()
        .map(|s| s.slug.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let path_w = sessions
        .iter()
        .map(|s| s.vcs.worktree_path.display().to_string().len())
        .max()
        .unwrap_or(8)
        .max(8);

    println!(
        "{:<slug_w$}  {:<9}  {:<4}  {:<path_w$}  {:<10}  CREATED",
        "SLUG", "CONTAINER", "AUTO", "WORKTREE", "WINDOW",
    );
    println!("{}", "-".repeat(slug_w + 9 + 4 + path_w + 10 + 19 + 10));

    for s in &sessions {
        let container = s
            .container
            .as_ref()
            .map(|c| c.runtime.as_str())
            .unwrap_or("—");
        let auto = if s.auto { "yes" } else { "—" };
        let created = s.created_at.format("%Y-%m-%d %H:%M").to_string();
        println!(
            "{:<slug_w$}  {:<9}  {:<4}  {:<path_w$}  {:<10}  {}",
            s.slug,
            container,
            auto,
            s.vcs.worktree_path.display(),
            s.tmux.tmux_window,
            created,
        );
    }
    Ok(())
}

fn cmd_list_all() -> anyhow::Result<()> {
    let home_dir = std::env::var_os("HOME").map(std::path::PathBuf::from);

    let sessions = session::load_all_sessions()?;

    if sessions.is_empty() {
        println!("No sessions found across any repo.");
        return Ok(());
    }

    // Sort: non-stale first (by repo_root, then created_at), stale last.
    let mut sessions = sessions;
    sessions.sort_by(|a, b| {
        let a_stale = !a.repo_root.exists();
        let b_stale = !b.repo_root.exists();
        a_stale
            .cmp(&b_stale)
            .then_with(|| a.repo_root.cmp(&b.repo_root))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });

    let abbrev_home = |p: &std::path::Path| -> String {
        if let Some(ref home) = home_dir {
            if let Ok(rel) = p.strip_prefix(home) {
                return format!("~/{}", rel.display());
            }
        }
        p.display().to_string()
    };

    let slug_w = sessions
        .iter()
        .map(|s| s.slug.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let repo_w = sessions
        .iter()
        .map(|s| abbrev_home(&s.repo_root).len())
        .max()
        .unwrap_or(4)
        .max(4);
    let path_w = sessions
        .iter()
        .map(|s| s.vcs.worktree_path.display().to_string().len())
        .max()
        .unwrap_or(8)
        .max(8);

    println!(
        "{:<repo_w$}  {:<slug_w$}  {:<9}  {:<4}  {:<path_w$}  {:<10}  {:<7}  CREATED",
        "REPO", "SLUG", "CONTAINER", "AUTO", "WORKTREE", "WINDOW", "STATUS",
    );
    println!("{}", "-".repeat(repo_w + slug_w + 9 + 4 + path_w + 10 + 7 + 19 + 14));

    for s in &sessions {
        let container = s
            .container
            .as_ref()
            .map(|c| c.runtime.as_str())
            .unwrap_or("—");
        let auto = if s.auto { "yes" } else { "—" };
        let created = s.created_at.format("%Y-%m-%d %H:%M").to_string();
        let status = if s.repo_root.exists() { "" } else { "stale" };
        let repo = abbrev_home(&s.repo_root);
        println!(
            "{:<repo_w$}  {:<slug_w$}  {:<9}  {:<4}  {:<path_w$}  {:<10}  {:<7}  {}",
            repo,
            s.slug,
            container,
            auto,
            s.vcs.worktree_path.display(),
            s.tmux.tmux_window,
            status,
            created,
        );
    }
    Ok(())
}

// ── am attach ────────────────────────────────────────────────────────────────

fn cmd_attach(slug: &str) -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;
    if let Err(e) = session::migrate_sessions(&repo_root) {
        eprintln!("warning: session migration failed: {e}");
    }
    let sessions = session::load_sessions_for_repo(&repo_root)?;

    let mut s = session::find_session(&sessions, slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?
        .clone();

    if !tmux::is_in_tmux() {
        return Err(error::AmError::NotInTmux.into());
    }

    let window_name = &s.tmux.tmux_window;
    let window_target = s.tmux.tmux_window_id.as_deref().unwrap_or(window_name);

    // Try to switch to an existing window; if it's not there, create it.
    if tmux::select_window(window_target).is_err() {
        let project_config_path = repo_root.join(".am").join("config.toml");
        let cfg = config::load_with_global(
            config::global_config_path().as_deref(),
            Some(&project_config_path),
        )?;
        let window_id = tmux::create_window(window_name, &s.vcs.worktree_path)
            .map_err(|e| anyhow::anyhow!(
                "{e}\nHint: a window named '{window_name}' may already exist — run 'am destroy {slug}' first"
            ))?;
        let (agent_pane_idx, shell_pane_idx, split_before) = pane_layout(&cfg.tmux.agent_pane);
        tmux::split_window(
            &window_id,
            &s.vcs.worktree_path,
            &cfg.tmux.split,
            cfg.tmux.split_percent,
            split_before,
            None,
        )?;
        tmux::select_pane(&tmux::get_pane_id(&window_id, shell_pane_idx))?;
        tmux::select_window(&window_id)?;
        s.tmux.tmux_window_id = Some(window_id.clone());
        s.tmux.agent_pane = tmux::get_pane_id(&window_id, agent_pane_idx);
        s.tmux.shell_pane = tmux::get_pane_id(&window_id, shell_pane_idx);
        session::update_session_global(s.clone())?;
        println!("Opened new window for session '{slug}'.");
        if s.container.is_some() {
            println!("  Note: the container was stopped when the window closed.");
            println!("  To restart cleanly: am destroy --force {slug} && am start {slug}");
        }
    } else {
        println!("Attached to session '{slug}'.");
    }
    Ok(())
}

// ── am run ────────────────────────────────────────────────────────────────────

fn cmd_run(slug: &str, agent: &str) -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;
    if let Err(e) = session::migrate_sessions(&repo_root) {
        eprintln!("warning: session migration failed: {e}");
    }
    let sessions = session::load_sessions_for_repo(&repo_root)?;

    let s = session::find_session(&sessions, slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?;

    if !tmux::is_in_tmux() {
        return Err(error::AmError::NotInTmux.into());
    }

    tmux::send_keys(&s.tmux.agent_pane, agent)?;
    tmux::select_window(s.tmux.tmux_window_id.as_deref().unwrap_or(&s.tmux.tmux_window))?;
    println!("Launched '{agent}' in session '{slug}'.");
    Ok(())
}

// ── am destroy ───────────────────────────────────────────────────────────────

fn cmd_destroy(slug: &str, force: bool, msgs: &messages::Messages) -> anyhow::Result<()> {
    let (repo_root, vcs) = find_repo_root()?;
    if let Err(e) = session::migrate_sessions(&repo_root) {
        eprintln!("warning: session migration failed: {e}");
    }
    let sessions = session::load_sessions_for_repo(&repo_root)?;

    let s = session::find_session(&sessions, slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?;

    if !force {
        // Warn about uncommitted changes in git worktrees only.
        if matches!(vcs, config::Vcs::Git)
            && worktree::git_worktree_has_changes(&s.vcs.worktree_path)
        {
            eprintln!(
                "\x1b[31mWarning: the worktree has uncommitted changes that will be lost.\x1b[0m"
            );
        }
        print!("{}", msgs.destroy_confirm(slug));
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Stop and remove container if one was recorded for this session
    if let Some(ref sc) = s.container {
        let pref = match sc.runtime.as_str() {
            "docker" => config::RuntimePreference::Docker,
            _ => config::RuntimePreference::Podman,
        };
        if let Ok(rt) = container::detect_runtime(pref) {
            let container_name = sc.container_name.as_deref().unwrap_or(&s.tmux.tmux_window);
            let _ = container::stop_container(&rt, container_name);
            let _ = container::remove_container(&rt, container_name);
        }
    }

    // Clean up the tmux window (ignore errors — window/pane may not exist)
    {
        if s.tmux.original_window_name.is_some() {
            // New-style session: cd shell pane back, kill agent pane, restore window name.
            if let Some(ref orig_dir) = s.tmux.original_shell_dir {
                let _ = tmux::send_keys(&s.tmux.shell_pane, &cd_cmd(orig_dir));
            }
            let _ = tmux::kill_pane(&s.tmux.agent_pane);
            if let Some(ref orig) = s.tmux.original_window_name {
                let target = s.tmux.tmux_window_id.as_deref().unwrap_or(&s.tmux.tmux_window);
                let _ = tmux::rename_window(Some(target), orig);
            }
        } else {
            // Old-style session: the window was dedicated, kill it entirely.
            let target = s.tmux.tmux_window_id.as_deref().unwrap_or(&s.tmux.tmux_window);
            let _ = tmux::kill_window(target);
        }
    }

    // Remove worktree — fail hard so the session record is preserved if
    // cleanup fails (otherwise the workspace becomes untracked/orphaned).
    // Use --force to skip worktree removal and delete the session record anyway.
    let remove_result = match vcs {
        config::Vcs::Git => worktree::remove_git_worktree(slug, &repo_root),
        config::Vcs::Jj => worktree::remove_jj_workspace(slug, &repo_root),
    };
    if let Err(e) = remove_result {
        if force {
            eprintln!("warning: could not fully remove worktree: {e}");
        } else {
            return Err(e.context(
                "worktree removal failed; session record preserved. \
                Re-run with --force to skip worktree cleanup and remove the session anyway.",
            ));
        }
    }

    // Remove session record
    session::remove_session_global(&repo_root, slug)?;

    println!("Destroyed session '{slug}'.");
    Ok(())
}

// ── am doctor ─────────────────────────────────────────────────────────────────

/// Report readiness without changing anything.
///
/// Exits non-zero when something would stop `am start` from working, so it is usable as a
/// setup gate in a script. Warnings alone do not fail.
fn cmd_doctor(agent_flag: Option<&str>) -> anyhow::Result<()> {
    let repo = find_repo_root().ok();
    let report = doctor::run(
        repo.as_ref().map(|(root, vcs)| (root.as_path(), vcs.clone())),
        agent_flag,
    );
    print!("{}", report.render());
    if report.failures() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ── am session rm ─────────────────────────────────────────────────────────────

fn cmd_session_rm(slug: &str, repo: Option<&std::path::Path>, force: bool) -> anyhow::Result<()> {
    // Resolve repo_root.
    let repo_root: std::path::PathBuf = if let Some(r) = repo {
        // Use provided --repo path as-is (need not exist — could be stale).
        if r.is_absolute() {
            r.to_path_buf()
        } else {
            std::env::current_dir()?.join(r)
        }
    } else {
        // No --repo: try to find the repo root from CWD.
        match find_repo_root() {
            Ok((root, _)) => {
                // We're inside a repo. Check if slug matches this repo.
                if let Err(e) = session::migrate_sessions(&root) {
                    eprintln!("warning: session migration failed: {e}");
                }
                let all = session::load_all_sessions()?;
                let for_this_repo: Vec<_> = all
                    .iter()
                    .filter(|s| s.repo_root == root && s.slug == slug)
                    .collect();
                // Look across all repos for this slug.
                let matching: Vec<_> = all
                    .iter()
                    .filter(|s| s.slug == slug)
                    .collect();
                if matching.is_empty() {
                    return Err(error::AmError::SlugNotFound(slug.to_string()).into());
                } else if matching.len() == 1 {
                    matching[0].repo_root.clone()
                } else if !for_this_repo.is_empty() && matching.len() == for_this_repo.len() {
                    // All matches are in the current repo — no ambiguity.
                    root
                } else {
                    let repos: Vec<_> = matching
                        .iter()
                        .map(|s| format!("  {}", s.repo_root.display()))
                        .collect();
                    return Err(anyhow::anyhow!(
                        "slug '{}' exists in multiple repos:\n{}\nUse --repo <path> to specify which one.",
                        slug,
                        repos.join("\n")
                    ));
                }
            }
            Err(_) => {
                // Not inside a repo — look across all sessions for this slug.
                let all = session::load_all_sessions()?;
                let matching: Vec<_> = all.iter().filter(|s| s.slug == slug).collect();
                if matching.is_empty() {
                    return Err(error::AmError::SlugNotFound(slug.to_string()).into());
                } else if matching.len() == 1 {
                    matching[0].repo_root.clone()
                } else {
                    let repos: Vec<_> = matching
                        .iter()
                        .map(|s| format!("  {}", s.repo_root.display()))
                        .collect();
                    return Err(anyhow::anyhow!(
                        "slug '{}' exists in multiple repos:\n{}\nUse --repo <path> to specify which one.",
                        slug,
                        repos.join("\n")
                    ));
                }
            }
        }
    };

    // Find the session in the global store.
    let all = session::load_all_sessions()?;
    let s = all
        .iter()
        .find(|s| s.repo_root == repo_root && s.slug == slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?
        .clone();

    // Confirmation prompt (unless --force).
    if !force {
        print!(
            "Remove session '{}' from {}? [y/N] ",
            slug,
            repo_root.display()
        );
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Best-effort container cleanup.
    if let Some(ref sc) = s.container {
        let pref = match sc.runtime.as_str() {
            "docker" => config::RuntimePreference::Docker,
            _ => config::RuntimePreference::Podman,
        };
        if let Ok(rt) = container::detect_runtime(pref) {
            let container_name = sc.container_name.as_deref().unwrap_or(&s.tmux.tmux_window);
            if let Err(e) = container::stop_container(&rt, container_name) {
                eprintln!("warning: container cleanup failed: {e}");
            }
            if let Err(e) = container::remove_container(&rt, container_name) {
                eprintln!("warning: container cleanup failed: {e}");
            }
        }
    }

    // Best-effort tmux cleanup.
    let window_target = s.tmux.tmux_window_id.as_deref().unwrap_or(&s.tmux.tmux_window);
    if let Err(e) = tmux::kill_window(window_target) {
        eprintln!("warning: tmux cleanup failed: {e}");
    }

    // Remove the record from the global store.
    session::remove_session_global(&repo_root, slug)?;

    println!("Removed session '{slug}'.");
    Ok(())
}

// ── am generate-config ────────────────────────────────────────────────────────

fn cmd_generate_config() -> anyhow::Result<()> {
    print!("{}", config::global_config_template());
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Produce a `cd '<path>'` command safe for use with tmux send-keys.
/// Single quotes in the path are escaped as `'\''` (POSIX shell quoting).
fn cd_cmd(path: &std::path::Path) -> String {
    let escaped = path.to_string_lossy().replace('\'', "'\\''");
    format!("cd '{escaped}'")
}

/// Map the configured agent side onto `(agent_pane_idx, shell_pane_idx, split_before)`.
///
/// The agent is *always* the pane `split-window` creates, so tmux can exec the container
/// command in it directly. The alternative — leaving the agent in the window's original
/// pane and typing the command in with send-keys — makes an interactive shell redraw a
/// several-hundred-character line, which renders it twice and drops the whole `podman run`
/// invocation into the user's shell history.
///
/// `-b` inserts the new pane before the existing one, which puts it left (horizontal split)
/// or above (vertical) *and* makes it pane index 0.
fn pane_layout(agent_pane: &config::PaneSide) -> (usize, usize, bool) {
    match agent_pane {
        config::PaneSide::Right => (1, 0, false),
        config::PaneSide::Left => (0, 1, true),
    }
}

fn read_git_config(key: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["config", "--global", key])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn find_repo_root() -> anyhow::Result<(PathBuf, config::Vcs)> {
    let mut dir = std::env::current_dir()?;
    loop {
        // .jj check first: colocated jj+git repos have both .jj and .git.
        // In the main repo, .jj/repo is a directory (the object store).
        // In a workspace, .jj/repo is a symlink — keep walking up.
        if dir.join(".jj").is_dir() && dir.join(".jj").join("repo").is_dir() {
            return Ok((dir, config::Vcs::Jj));
        }
        // .git in a git worktree is a FILE pointing back to the main repo;
        // only stop when we find it as a DIRECTORY (the actual repo root).
        if dir.join(".git").is_dir() {
            return Ok((dir, config::Vcs::Git));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Err(error::AmError::NotInRepo.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pane_layout;
    use crate::command::shell_quote;
    use crate::config::PaneSide;

    #[test]
    fn pane_layout_puts_agent_in_the_new_pane_on_the_right() {
        assert_eq!(pane_layout(&PaneSide::Right), (1, 0, false));
    }

    #[test]
    fn pane_layout_puts_agent_in_the_new_pane_on_the_left() {
        // -b, so the new pane is inserted first and becomes index 0.
        assert_eq!(pane_layout(&PaneSide::Left), (0, 1, true));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_quote_leaves_safe_tokens_unquoted() {
        assert_eq!(shell_quote("podman"), "podman");
        assert_eq!(shell_quote("/tmp/foo:rw"), "/tmp/foo:rw");
    }

    // ── Agent injection ───────────────────────────────────────────────────────

    use super::{agent_command, injected_features};
    use crate::config::{AgentInstall, Config};
    use crate::container::KnownAgent;

    fn cfg_with_install(install: AgentInstall) -> Config {
        let mut cfg = Config::default();
        cfg.devcontainer.agent_install = install;
        cfg
    }

    fn ids(cfg: &Config, agent: Option<&str>) -> Vec<String> {
        injected_features(cfg, agent)
            .into_iter()
            .map(|f| f.id)
            .collect()
    }

    #[test]
    fn auto_injects_the_agents_feature_when_one_is_mapped() {
        let cfg = cfg_with_install(AgentInstall::Auto);
        assert_eq!(
            ids(&cfg, Some("claude")),
            vec!["ghcr.io/anthropics/devcontainer-features/claude-code:1"]
        );
    }

    #[test]
    fn auto_injects_nothing_for_an_agent_with_no_published_feature() {
        // copilot has no official Feature; it falls through to the bootstrap path.
        let cfg = cfg_with_install(AgentInstall::Auto);
        assert!(ids(&cfg, Some("copilot")).is_empty());
    }

    #[test]
    fn install_none_injects_nothing_even_when_a_feature_exists() {
        let cfg = cfg_with_install(AgentInstall::None);
        assert!(ids(&cfg, Some("claude")).is_empty());
    }

    #[test]
    fn bootstrap_does_not_inject_a_build_time_feature() {
        let cfg = cfg_with_install(AgentInstall::Bootstrap);
        assert!(ids(&cfg, Some("claude")).is_empty());
    }

    #[test]
    fn extra_features_are_injected_alongside_the_agents() {
        let mut cfg = cfg_with_install(AgentInstall::Auto);
        cfg.devcontainer
            .extra_features
            .insert("ghcr.io/devcontainers/features/node:1".to_string(), "{}".to_string());
        let ids = ids(&cfg, Some("claude"));
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"ghcr.io/devcontainers/features/node:1".to_string()));
    }

    #[test]
    fn injected_features_are_ordered_deterministically() {
        // The image name is derived from these, so an unstable order would produce a
        // different hash — and a needless rebuild — on every run.
        let mut cfg = cfg_with_install(AgentInstall::Auto);
        for id in ["zzz:1", "aaa:1", "mmm:1"] {
            cfg.devcontainer
                .extra_features
                .insert(id.to_string(), "{}".to_string());
        }
        assert_eq!(ids(&cfg, Some("claude")), ids(&cfg, Some("claude")));
        let sorted = {
            let mut v = ids(&cfg, Some("claude"));
            v.sort();
            v
        };
        assert_eq!(ids(&cfg, Some("claude")), sorted);
    }

    #[test]
    fn no_agent_means_no_agent_feature() {
        let cfg = cfg_with_install(AgentInstall::Auto);
        assert!(ids(&cfg, None).is_empty());
    }

    // ── Agent command ─────────────────────────────────────────────────────────

    #[test]
    fn agent_command_is_empty_without_an_agent() {
        assert!(agent_command(None, false).is_empty());
    }

    #[test]
    fn agent_command_adds_auto_flags_only_in_auto_mode() {
        assert_eq!(agent_command(Some(KnownAgent::Claude), false), vec!["claude"]);
        assert_eq!(
            agent_command(Some(KnownAgent::Claude), true),
            vec!["claude", "--dangerously-skip-permissions"]
        );
    }

    // ── container_name ───────────────────────────────────────────────────────

    use super::container_name;
    use std::path::Path;

    #[test]
    fn container_name_includes_a_hash() {
        let name = container_name(Path::new("/repo-a"), "feat");
        assert!(name.starts_with("am-feat-"), "expected hash suffix, got: {name}");
        assert!(name.len() > "am-feat-".len(), "hash suffix should not be empty");
    }

    #[test]
    fn container_name_is_deterministic() {
        let a = container_name(Path::new("/repo-b"), "feat");
        let b = container_name(Path::new("/repo-b"), "feat");
        assert_eq!(a, b);
    }

    #[test]
    fn container_name_differs_across_repos() {
        let b = container_name(Path::new("/repo-b"), "feat");
        let c = container_name(Path::new("/repo-c"), "feat");
        assert_ne!(b, c);
    }

    #[test]
    fn container_name_differs_across_slugs() {
        let a = container_name(Path::new("/repo-a"), "feat");
        let b = container_name(Path::new("/repo-a"), "bugfix");
        assert_ne!(a, b);
    }
}
