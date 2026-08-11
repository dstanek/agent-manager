//! Terminal color, decided per stream and applied only where it carries meaning.
//!
//! One color means one severity everywhere in `am`: green for fine, yellow for something
//! worth reading, red for something that will stop you. Color is an accent on a glyph or
//! a prefix that already says the same thing in words — remove it and nothing is lost, so
//! a piped log, a `NO_COLOR` user, and a screen reader all still get the message.

use std::io::IsTerminal;

/// Which stream a message is going to. Color is decided per stream because they are
/// redirected independently — `am doctor > report.txt` should still color the warnings
/// that go to stderr.
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
}
