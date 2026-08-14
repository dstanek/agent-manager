//! Terminal color, decided per stream and applied only where it carries meaning.
//!
//! One color means one severity everywhere in `am`: green for fine, yellow for something
//! worth reading, red for something that will stop you. Color is an accent on a glyph or
//! a prefix that already says the same thing in words — remove it and nothing is lost, so
//! a piped log, a `NO_COLOR` user, and a screen reader all still get the message.

use std::io::IsTerminal;

/// Which stream a message is going to. Color is decided per stream because they are
/// redirected independently — `am start > report.txt` should still color a warning like
/// "removed existing container from a previous unclean run" that goes to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Whether to color output on `stream`.
///
/// Honours the [`NO_COLOR`](https://no-color.org/) convention and `CLICOLOR_FORCE`, which
/// between them cover the two cases detection gets wrong: a terminal the user does not
/// want colored, and a pipe that is going somewhere that renders ANSI anyway (`less -R`,
/// a CI log viewer).
pub fn enabled(stream: Stream) -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    }
}

/// The colors `am` uses. Deliberately the terminal's own 8-color palette rather than
/// fixed RGB, so they follow whatever theme the user has chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Green,
    Yellow,
    Red,
    /// Not a severity — de-emphasis, for text that is structure rather than content:
    /// table headers, and hints that belong to a finding already stated above them.
    Dim,
}

impl Color {
    fn code(self) -> &'static str {
        match self {
            Color::Green => "\x1b[32m",
            Color::Yellow => "\x1b[33m",
            Color::Red => "\x1b[31m",
            Color::Dim => "\x1b[2m",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// Wrap `text` in `color` when `enabled`, otherwise return it untouched.
///
/// Taking the flag as an argument rather than probing the terminal here keeps rendering
/// pure: callers decide once per stream, and tests can render both forms without touching
/// the environment.
pub fn paint(text: &str, color: Color, enabled: bool) -> String {
    if enabled {
        format!("{}{text}{RESET}", color.code())
    } else {
        text.to_string()
    }
}

// `am`'s three severity prefixes — the vocabulary every module reaches for when it has to
// tell the user something went wrong, might go wrong, or is worth knowing anyway. Kept here
// rather than duplicated (or hand-rolled uncoloured) per call site, so "warning:" reads the
// same word in the same colour everywhere it appears.
//
// Each takes `enabled` rather than probing a stream itself, for the same reason `paint` does:
// rendering stays pure, and a caller that already has its stream's colour decision in hand
// (or, for an interactive flow, already threaded one down from the top) reuses it instead of
// deciding twice.

/// The `error:` prefix, red when `enabled`. One of `am`'s three severity prefixes (with
/// [`warning_prefix`] and [`note_prefix`]) — see the module-level note above them for why
/// they're centralized and why `enabled` is a plain argument. Color goes on the label alone,
/// never the message: the words have to survive being piped into a log.
pub fn error_prefix(enabled: bool) -> String {
    paint("error:", Color::Red, enabled)
}

/// The `warning:` prefix, yellow when `enabled`. One of `am`'s three severity prefixes — see
/// [`error_prefix`] for the shared rationale. (For the one deliberate case where a warning
/// needs an error's visual weight, see [`warning_prefix_severe`].)
pub fn warning_prefix(enabled: bool) -> String {
    paint("warning:", Color::Yellow, enabled)
}

/// The `Note:` prefix, yellow when `enabled`. One of `am`'s three severity prefixes — see
/// [`error_prefix`] for the shared rationale. Capitalized, unlike the other two, to set a note
/// (which is informational, not a problem) apart from a warning or error at a glance.
pub fn note_prefix(enabled: bool) -> String {
    paint("Note:", Color::Yellow, enabled)
}

/// The `warning:` prefix, but **red** instead of yellow — the one deliberate exception to this
/// module's yellow-warning/red-error rule (see the module doc). Reserved for `am destroy`'s
/// uncommitted-changes warning: confirming destroys the worktree's uncommitted changes for
/// good, so it earns an error's visual weight despite being phrased as a warning. Not a
/// general-purpose "make this warning stand out" escape hatch — reach for [`warning_prefix`]
/// unless the case is, like that one, irreversible.
pub fn warning_prefix_severe(enabled: bool) -> String {
    paint("warning:", Color::Red, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_wraps_only_when_enabled() {
        assert_eq!(paint("ok", Color::Green, true), "\x1b[32mok\x1b[0m");
        assert_eq!(paint("ok", Color::Green, false), "ok");
    }

    #[test]
    fn each_color_has_a_distinct_code() {
        let green = paint("x", Color::Green, true);
        let yellow = paint("x", Color::Yellow, true);
        let red = paint("x", Color::Red, true);
        assert_ne!(green, yellow);
        assert_ne!(yellow, red);
        assert_ne!(green, red);
    }

    #[test]
    fn painted_text_still_contains_the_text() {
        // Color is an accent, never a replacement: the words have to survive it.
        assert!(paint("no image configured", Color::Red, true).contains("no image configured"));
    }

    #[test]
    fn severity_prefixes_are_uncolored_when_disabled() {
        assert_eq!(error_prefix(false), "error:");
        assert_eq!(warning_prefix(false), "warning:");
        assert_eq!(note_prefix(false), "Note:");
    }

    #[test]
    fn severity_prefixes_are_colored_when_enabled() {
        assert_eq!(error_prefix(true), "\x1b[31merror:\x1b[0m");
        assert_eq!(warning_prefix(true), "\x1b[33mwarning:\x1b[0m");
        assert_eq!(note_prefix(true), "\x1b[33mNote:\x1b[0m");
    }

    #[test]
    fn warning_prefix_severe_reads_warning_but_paints_red() {
        // Same word as `warning_prefix`, deliberately the error colour — see the doc comment
        // for why `am destroy` needs this one exception rather than plain `warning_prefix`.
        assert_eq!(warning_prefix_severe(false), "warning:");
        assert_eq!(warning_prefix_severe(true), "\x1b[31mwarning:\x1b[0m");
        assert_eq!(
            warning_prefix_severe(true),
            error_prefix(true).replace("error:", "warning:")
        );
    }
}
