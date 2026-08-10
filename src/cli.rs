use clap::{Parser, Subcommand};

fn validate_slug(s: &str) -> Result<String, String> {
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

    /// List all sessions
    List,

    /// Attach tmux focus to an existing session
    Attach {
        #[arg(value_parser = validate_slug)]
        slug: String,
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
}
