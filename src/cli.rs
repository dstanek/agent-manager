use std::path::PathBuf;

use clap::{Parser, Subcommand};

pub fn validate_slug(s: &str) -> Result<String, String> {
    if s.is_empty() || s.len() > 40 {
        return Err(format!("slug must be 1–40 characters (got {})", s.len()));
    }
    if !s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err("slug must start with a lowercase letter or digit".to_string());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(
            "slug may only contain lowercase letters, digits, underscores, and hyphens".to_string(),
        );
    }
    Ok(s.to_string())
}

#[derive(Parser)]
#[command(
    name = "am",
    about = "Agent Manager — isolated agent sessions",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize am in the current repo
    Init,

    /// Guided, interactive setup — init plus a short question flow plus verification
    Setup {
        /// Skip all prompts; use effective current values for every question
        #[arg(short, long)]
        yes: bool,
        /// Explicitly set (and, if it differs from what's there, write) the agent
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Start a new agent session
    Start {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Agent command to launch in the agent pane (overrides config)
        #[arg(short, long)]
        agent: Option<String>,
        /// Disable container isolation for this session (overrides config)
        #[arg(long)]
        no_container: bool,
        /// Run agent in autonomous mode (skips all tool approval prompts; requires container)
        #[arg(long)]
        auto: bool,
        /// Rebuild the devcontainer image even if one already exists for this config
        #[arg(long)]
        rebuild: bool,
    },

    /// Report what is and is not ready for a successful `am start`
    Doctor,

    /// List sessions (default: current repo; --all shows all repos)
    List {
        #[arg(long, help = "Show sessions from all repos")]
        all: bool,
    },

    /// Attach tmux focus to an existing session, restoring the agent if it isn't running
    Attach {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Skip resuming the previous conversation; launch/relaunch fresh
        #[arg(long)]
        fresh: bool,
    },

    /// Launch an agent in an existing session's agent pane
    Run {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Agent command to run, e.g. "claude", "codex"
        agent: String,
    },

    /// Destroy a session: remove worktree, kill tmux window, stop container
    Destroy {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Print a global config template with all options and defaults to stdout
    GenerateConfig,

    /// Manage session records in the global store
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
}

#[derive(Subcommand)]
pub enum SessionCommands {
    /// Remove a session record (and attempt container/tmux cleanup)
    Rm {
        #[arg(value_parser = validate_slug)]
        slug: String,
        /// Repository root containing this session (required when not inside a repo,
        /// or when the slug exists in multiple repos)
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_slug_accepted() {
        assert!(validate_slug("feat").is_ok());
        assert!(validate_slug("my-feature").is_ok());
        assert!(validate_slug("feature_123").is_ok());
        assert!(validate_slug("a").is_ok());
        assert!(validate_slug(&"a".repeat(40)).is_ok());
    }

    #[test]
    fn slug_too_long_rejected() {
        assert!(validate_slug(&"a".repeat(41)).is_err());
    }

    #[test]
    fn empty_slug_rejected() {
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn invalid_chars_rejected() {
        assert!(validate_slug("my feature").is_err()); // space
        assert!(validate_slug("MyFeature").is_err()); // uppercase
        assert!(validate_slug("feat!").is_err()); // special char
        assert!(validate_slug("feat/sub").is_err()); // slash
    }

    #[test]
    fn slug_must_start_with_alphanumeric() {
        assert!(validate_slug("-leading-dash").is_err());
        assert!(validate_slug("_leading_underscore").is_err());
        assert!(validate_slug("a-leading-letter").is_ok());
        assert!(validate_slug("1-leading-digit").is_ok());
    }

    // ── am setup ──────────────────────────────────────────────────────────────

    fn parse(args: &[&str]) -> Commands {
        Cli::try_parse_from(args).expect("args should parse").command
    }

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn setup_defaults_to_interactive_with_no_agent() {
        match parse(&["am", "setup"]) {
            Commands::Setup { yes, agent } => {
                assert!(!yes);
                assert_eq!(agent, None);
            }
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_accepts_yes_in_both_forms() {
        for args in [&["am", "setup", "--yes"], &["am", "setup", "-y"]] {
            match parse(args) {
                Commands::Setup { yes, .. } => assert!(yes, "{args:?} should set yes"),
                _ => panic!("expected Setup"),
            }
        }
    }

    #[test]
    fn setup_accepts_agent_in_both_forms() {
        for args in [
            &["am", "setup", "--agent", "claude"],
            &["am", "setup", "-a", "claude"],
        ] {
            match parse(args) {
                Commands::Setup { agent, .. } => assert_eq!(agent.as_deref(), Some("claude")),
                _ => panic!("expected Setup"),
            }
        }
    }

    #[test]
    fn setup_accepts_yes_and_agent_together() {
        // The documented CI form: bootstrap and deterministically set the agent.
        match parse(&["am", "setup", "--yes", "--agent", "codex"]) {
            Commands::Setup { yes, agent } => {
                assert!(yes);
                assert_eq!(agent.as_deref(), Some("codex"));
            }
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_does_not_validate_the_agent_name() {
        // Unknown names are rejected later, by KnownAgent::parse, so the error message
        // lists the valid agents instead of clap's generic one.
        match parse(&["am", "setup", "--agent", "not-an-agent"]) {
            Commands::Setup { agent, .. } => assert_eq!(agent.as_deref(), Some("not-an-agent")),
            _ => panic!("expected Setup"),
        }
    }

    #[test]
    fn setup_takes_no_positional_arguments() {
        assert!(Cli::try_parse_from(["am", "setup", "feat"]).is_err());
    }

    #[test]
    fn init_still_parses_with_no_arguments() {
        assert!(matches!(parse(&["am", "init"]), Commands::Init));
    }

    // ── am attach ─────────────────────────────────────────────────────────────

    #[test]
    fn attach_defaults_to_no_fresh() {
        match parse(&["am", "attach", "feat"]) {
            Commands::Attach { slug, fresh } => {
                assert_eq!(slug, "feat");
                assert!(!fresh);
            }
            _ => panic!("expected Attach"),
        }
    }

    #[test]
    fn attach_accepts_fresh_flag() {
        match parse(&["am", "attach", "feat", "--fresh"]) {
            Commands::Attach { fresh, .. } => assert!(fresh),
            _ => panic!("expected Attach"),
        }
    }
}
