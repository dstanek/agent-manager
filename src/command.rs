use std::error::Error;
use std::process::Output;

use anyhow::Result;

/// Shared output-checking logic: returns an error if the command failed,
/// otherwise returns stdout bytes for the caller to interpret.
fn check_output<E>(display: &str, output: Output, error_fn: impl Fn(String) -> E) -> Result<Vec<u8>>
where
    E: Error + Send + Sync + 'static,
{
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("{display} exited with status {}", output.status)
        } else {
            stderr
        };
        return Err(error_fn(msg).into());
    }
    Ok(output.stdout)
}

/// Execute a pre-built `Command` and return success/failure.
///
/// Prefer this over `run_command` when arguments include `Path`s — callers
/// can use `.arg(path)` directly without converting to `&str` first.
pub fn run_built_command<E>(
    mut cmd: std::process::Command,
    error_fn: impl Fn(String) -> E,
) -> Result<()>
where
    E: Error + Send + Sync + 'static,
{
    let display = format!("{cmd:?}");
    let output = cmd
        .output()
        .map_err(|e| error_fn(format!("failed to run {display}: {e}")))?;
    check_output(&display, output, error_fn)?;
    Ok(())
}

/// Execute a pre-built `Command` and return stdout.
///
/// Building the `Command` yourself keeps `Path`/`OsStr` arguments intact instead
/// of forcing an early lossy conversion to `&str`.
pub fn run_built_command_output<E>(
    mut cmd: std::process::Command,
    error_fn: impl Fn(String) -> E,
) -> Result<String>
where
    E: Error + Send + Sync + 'static,
{
    let display = format!("{cmd:?}");
    let output = cmd
        .output()
        .map_err(|e| error_fn(format!("failed to run {display}: {e}")))?;
    let stdout = check_output(&display, output, error_fn)?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

/// Quote a single token for a POSIX shell, leaving already-safe tokens bare.
///
/// Used both for `tmux send-keys` lines and for the `sh -c` script that chains
/// devcontainer entrypoints, so the escaping rule lives in one place.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '/' | ':' | '=' | '+' | '@' | '%')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Execute a command and return success/failure.
///
/// The `error_fn` closure is called to construct an error if the command
/// fails, allowing each caller to produce their own error type.
pub fn run_command<E>(bin: &str, args: &[&str], error_fn: impl Fn(String) -> E) -> Result<()>
where
    E: Error + Send + Sync + 'static,
{
    let output = std::process::Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| error_fn(format!("failed to run {bin}: {e}")))?;
    check_output(bin, output, error_fn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct TestError(String);

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl Error for TestError {}

    #[test]
    fn run_command_success() {
        let result = run_command("true", &[], |msg| TestError(msg));
        assert!(result.is_ok());
    }

    #[test]
    fn run_command_failure() {
        let result = run_command("false", &[], |msg| TestError(msg));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("status"));
    }

    #[test]
    fn run_command_error_includes_binary_name() {
        let result = run_command("nonexistent-binary-xyz", &[], |msg| TestError(msg));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("nonexistent-binary-xyz"));
    }

    #[test]
    fn run_built_command_success() {
        let cmd = std::process::Command::new("true");
        assert!(run_built_command(cmd, TestError).is_ok());
    }

    #[test]
    fn run_built_command_failure() {
        let cmd = std::process::Command::new("false");
        assert!(run_built_command(cmd, TestError).is_err());
    }

    #[test]
    fn run_built_command_output_success() {
        let mut cmd = std::process::Command::new("echo");
        cmd.arg("hello");
        let result = run_built_command_output(cmd, TestError);
        assert_eq!(result.unwrap(), "hello");
    }

    #[test]
    fn run_built_command_output_failure() {
        let cmd = std::process::Command::new("false");
        assert!(run_built_command_output(cmd, TestError).is_err());
    }
}
