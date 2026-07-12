//! Context-aware user-facing wording.
//!
//! Some messages reference tmux constructs ("window", "pane") that only make
//! sense when `am` is running inside a tmux session. `Messages` selects the
//! right wording once at startup based on [`tmux::is_in_tmux`], so command
//! handlers never branch on tmux state just to pick a string.

use crate::tmux;

/// Chooses user-facing wording based on whether we are running inside tmux.
pub struct Messages {
    in_tmux: bool,
}

impl Messages {
    /// Build from the current environment (reads `$TMUX` once).
    pub fn from_env() -> Self {
        Self {
            in_tmux: tmux::is_in_tmux(),
        }
    }

    /// Confirmation prompt shown by `am destroy` before removing a session.
    /// Only mentions the tmux window when a window will actually be torn down.
    pub fn destroy_confirm(&self, slug: &str) -> String {
        if self.in_tmux {
            format!("Remove worktree and kill tmux window for '{slug}'? [y/N] ")
        } else {
            format!("Remove worktree for '{slug}'? [y/N] ")
        }
    }
}

#[cfg(test)]
impl Messages {
    /// Construct with an explicit tmux state for tests.
    pub fn with_in_tmux(in_tmux: bool) -> Self {
        Self { in_tmux }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destroy_confirm_mentions_window_in_tmux() {
        let m = Messages::with_in_tmux(true);
        let prompt = m.destroy_confirm("feat");
        assert!(prompt.contains("tmux window"));
        assert!(prompt.contains("feat"));
    }

    #[test]
    fn destroy_confirm_omits_tmux_wording_outside_tmux() {
        let m = Messages::with_in_tmux(false);
        let prompt = m.destroy_confirm("feat");
        assert!(!prompt.contains("window"));
        assert!(!prompt.contains("pane"));
        assert!(prompt.contains("feat"));
    }
}
