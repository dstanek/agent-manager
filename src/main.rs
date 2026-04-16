mod cli;
mod command;
mod config;
mod container;
mod error;
mod session;
mod tmux;
mod worktree;

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Start {
            slug,
            agent,
            no_container,
            auto,
        } => cmd_start(&slug, agent.as_deref(), no_container, auto),
        Commands::List => cmd_list(),
        Commands::Attach { slug } => cmd_attach(&slug),
        Commands::Run { slug, agent } => cmd_run(&slug, &agent),
        Commands::Destroy { slug, force } => cmd_destroy(&slug, force),
        Commands::GenerateConfig => cmd_generate_config(),
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

    let sessions_path = am_dir.join("sessions.json");
    if !sessions_path.exists() {
        std::fs::write(&sessions_path, "{\"sessions\":[]}\n")?;
        println!("Created .am/sessions.json");
    }

    let gitconfig_path = am_dir.join("gitconfig");
    if !gitconfig_path.exists() {
        let name = read_git_config("user.name").unwrap_or_default();
        let email = read_git_config("user.email").unwrap_or_default();
        let content = format!("[user]\n\tname = {name}\n\temail = {email}\n");
        std::fs::write(&gitconfig_path, content)?;
        println!("Created .am/gitconfig");
    } else {
        println!(".am/gitconfig already exists, skipping");
    }

    let gitignore_path = repo_root.join(".gitignore");
    let already_ignored = if gitignore_path.exists() {
        let content = std::fs::read_to_string(&gitignore_path)?;
        content
            .lines()
            .any(|l| l.trim() == ".am/" || l.trim() == ".am")
    } else {
        false
    };
    if !already_ignored {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore_path)?;
        file.write_all(b".am/\n")?;
        println!("Added .am/ to .gitignore");
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
) -> anyhow::Result<()> {
    let (repo_root, vcs) = find_repo_root()?;
    let sessions = session::load_sessions(&repo_root)?;

    if session::find_session(&sessions, slug).is_some() {
        return Err(error::AmError::SlugAlreadyExists(slug.to_string()).into());
    }

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

    // 4. Require am init when using containers
    if cfg.container.enabled && !no_container {
        let gitconfig_path = repo_root.join(".am").join("gitconfig");
        if !gitconfig_path.exists() && cfg.container.gitconfig.is_none() {
            return Err(anyhow::anyhow!(
                ".am/gitconfig not found — run 'am init' first to create the project configuration"
            ));
        }
    }

    // 5. Container config
    let use_container = cfg.container.enabled && !no_container;
    let (_runtime, container_cmd, session_container) = if use_container {
        let agent_auth = if let Some(agent) = effective_known_agent {
            container::preflight_agent_auth(agent, &cfg.container.user)?
        } else {
            container::AgentAuth::default()
        };

        let image = config::resolve_image(effective_agent.as_deref(), &cfg)
            .ok_or(error::AmError::ContainerImageNotConfigured)?;

        let runtime = container::detect_runtime(cfg.container.runtime.clone())?;

        // Pre-emptively remove any leftover container from a previous run
        container::remove_if_exists(&runtime, &format!("am-{slug}"));

        let mounts = container::resolve_mounts(
            slug,
            &repo_root,
            &vcs,
            agent_auth.mounts.clone(),
            cfg.container.gitconfig.as_deref(),
            cfg.container.ssh.as_deref(),
            &cfg.container.user,
        )?;

        let mut cmd = container::build_run_command(
            &runtime,
            image,
            &mounts,
            &cfg.container.env,
            &agent_auth.env,
            &cfg.container.network,
            &format!("am-{slug}"),
        );

        // Append the agent as the container CMD so it launches automatically
        if let Some(agent) = effective_known_agent {
            cmd.push(agent.to_string());
            if auto {
                cmd.extend(container::agent_auto_flags(agent));
            }
        }

        let sc = session::SessionContainer {
            runtime: runtime.kind.to_string(),
            image: image.to_string(),
            container_id: None,
        };
        (Some(runtime), Some(cmd), Some(sc))
    } else {
        (None, None, None)
    };

    let worktree_path = match vcs {
        config::Vcs::Git => worktree::create_git_worktree(slug, &repo_root)?,
        config::Vcs::Jj => worktree::create_jj_workspace(slug, &repo_root)?,
    };
    let window_name = format!("am-{slug}");

    // Pane assignment: split-window creates a new pane at index 1.
    // PaneSide::Right: agent in new pane (1), shell in original (0).
    // PaneSide::Left:  agent in original pane (0), shell in new pane (1).
    // The -p percent controls the size of the new pane.
    let (agent_pane_idx, shell_pane_idx, new_pane_percent) = match cfg.tmux.agent_pane {
        config::PaneSide::Right => (1usize, 0usize, cfg.tmux.split_percent),
        config::PaneSide::Left => (0usize, 1usize, 100 - cfg.tmux.split_percent),
    };

    let (original_window_name, original_shell_dir) = if tmux::is_in_tmux() {
        // Capture the current window name and pane path before we rename/split.
        let orig_window = tmux::current_window_name().ok().filter(|s| !s.is_empty());
        let orig_dir = tmux::current_pane_path().ok();
        tmux::rename_window(None, &window_name)
            .map_err(|e| anyhow::anyhow!(
                "{e}\nHint: a window named '{window_name}' may already exist — run 'am destroy {slug}' first"
            ))?;
        let container_shell_cmd = container_cmd
            .as_ref()
            .map(|cmd| cmd.iter().map(|s| shell_quote(s)).collect::<Vec<_>>().join(" "));
        tmux::split_window(
            &window_name,
            &worktree_path,
            &cfg.tmux.split,
            new_pane_percent,
            container_shell_cmd.as_deref(),
        )?;
        if container_cmd.is_none() {
            if let Some(ref agent) = effective_agent {
                tmux::send_keys(&tmux::get_pane_id(&window_name, agent_pane_idx), agent)?;
            }
        }
        // cd the shell pane into the worktree.
        tmux::send_keys(
            &tmux::get_pane_id(&window_name, shell_pane_idx),
            &cd_cmd(&worktree_path),
        )?;
        // Keep focus on the shell pane.
        tmux::select_pane(&tmux::get_pane_id(&window_name, shell_pane_idx))?;
        (orig_window, orig_dir)
    } else {
        if container_cmd.is_none() {
            println!("Note: not inside tmux — no window opened. Run 'am attach {slug}' from inside tmux to open one.");
        }
        (None, None)
    };

    let new_session = session::Session {
        slug: slug.to_string(),
        created_at: chrono::Utc::now(),
        auto,
        vcs: session::VcsMetadata {
            branch: format!("am/{slug}"),
            worktree_path: worktree_path.clone(),
        },
        tmux: session::TmuxMetadata {
            tmux_window: window_name,
            agent_pane: tmux::get_pane_id(&format!("am-{slug}"), agent_pane_idx),
            shell_pane: tmux::get_pane_id(&format!("am-{slug}"), shell_pane_idx),
            original_window_name,
            original_shell_dir,
        },
        container: session_container,
    };

    // Not in tmux with a container: record the session then replace this process.
    // Recording before exec ensures the session is always tracked; if exec fails
    // the user can run 'am destroy <slug>' to clean up.
    if let Some(ref cmd) = container_cmd {
        if !tmux::is_in_tmux() {
            session::add_session(&repo_root, new_session)?;
            println!("Started session '{slug}'");
            println!("  worktree:  {}", worktree_path.display());
            println!("  container: am-{slug}");
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
                // exec() only returns on failure
                return Err(error::AmError::ContainerError(format!(
                    "failed to exec container: {err}"
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

    session::add_session(&repo_root, new_session)?;

    println!("Started session '{slug}'");
    println!("  worktree:  {}", worktree_path.display());
    println!("  branch:    am/{slug}");
    if use_container {
        println!("  container: am-{slug}");
    }
    Ok(())
}

// ── am list ───────────────────────────────────────────────────────────────────

fn cmd_list() -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;
    let sessions = session::load_sessions(&repo_root)?;

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

// ── am attach ────────────────────────────────────────────────────────────────

fn cmd_attach(slug: &str) -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;
    let sessions = session::load_sessions(&repo_root)?;

    let s = session::find_session(&sessions, slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?;

    if !tmux::is_in_tmux() {
        return Err(error::AmError::NotInTmux.into());
    }

    let window_name = format!("am-{slug}");

    // Try to switch to an existing window; if it's not there, create it.
    if tmux::select_window(&window_name).is_err() {
        let project_config_path = repo_root.join(".am").join("config.toml");
        let cfg = config::load_with_global(
            config::global_config_path().as_deref(),
            Some(&project_config_path),
        )?;
        tmux::create_window(&window_name, &s.vcs.worktree_path)
            .map_err(|e| anyhow::anyhow!(
                "{e}\nHint: a window named '{window_name}' may already exist — run 'am destroy {slug}' first"
            ))?;
        let (shell_pane_idx, new_pane_percent) = match cfg.tmux.agent_pane {
            config::PaneSide::Right => (0usize, cfg.tmux.split_percent),
            config::PaneSide::Left => (1usize, 100 - cfg.tmux.split_percent),
        };
        tmux::split_window(
            &window_name,
            &s.vcs.worktree_path,
            &cfg.tmux.split,
            new_pane_percent,
            None,
        )?;
        tmux::select_pane(&tmux::get_pane_id(&window_name, shell_pane_idx))?;
        tmux::select_window(&window_name)?;
        println!("Opened new window for session '{slug}'.");
    } else {
        println!("Attached to session '{slug}'.");
    }
    Ok(())
}

// ── am run ────────────────────────────────────────────────────────────────────

fn cmd_run(slug: &str, agent: &str) -> anyhow::Result<()> {
    let (repo_root, _) = find_repo_root()?;
    let sessions = session::load_sessions(&repo_root)?;

    let s = session::find_session(&sessions, slug)
        .ok_or_else(|| error::AmError::SlugNotFound(slug.to_string()))?;

    if !tmux::is_in_tmux() {
        return Err(error::AmError::NotInTmux.into());
    }

    tmux::send_keys(&s.tmux.agent_pane, agent)?;
    tmux::select_window(&s.tmux.tmux_window)?;
    println!("Launched '{agent}' in session '{slug}'.");
    Ok(())
}

// ── am destroy ───────────────────────────────────────────────────────────────

fn cmd_destroy(slug: &str, force: bool) -> anyhow::Result<()> {
    let (repo_root, vcs) = find_repo_root()?;
    let sessions = session::load_sessions(&repo_root)?;

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
        print!("Remove worktree and kill tmux window for '{slug}'? [y/N] ");
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
            let _ = container::stop_container(&rt, &format!("am-{slug}"));
            let _ = container::remove_container(&rt, &format!("am-{slug}"));
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
                let _ = tmux::rename_window(Some(&s.tmux.tmux_window), orig);
            }
        } else {
            // Old-style session: the window was dedicated, kill it entirely.
            let _ = tmux::kill_window(&s.tmux.tmux_window);
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
    session::remove_session(&repo_root, slug)?;

    println!("Destroyed session '{slug}'.");
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

/// Quote a single shell token only when it contains characters that require quoting.
/// Safe characters (alphanumeric, common punctuation in paths/flags) are left bare.
fn shell_quote(s: &str) -> String {
    let needs_quoting = s.is_empty()
        || s.chars().any(|c| {
            !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '/' | ':' | '=' | '+' | '@' | '%')
        });
    if needs_quoting {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
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
