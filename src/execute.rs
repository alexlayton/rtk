//! Embeddable command execution with RTK filtering.
//!
//! The initial router deliberately handles only shell commands that can be
//! represented faithfully as a direct process invocation. Unsupported tools
//! and shell syntax are executed unchanged by the platform shell.

use crate::core::stream::status_to_exit_code;
use crate::core::utils::decode_process_output;
use crate::shell_lexer::{self, TokenKind};
use crate::wc_cmd;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Options controlling an embedded command execution.
#[derive(Debug, Clone, Default)]
pub struct ExecuteOptions {
    /// Directory in which the child command runs. The process's current
    /// directory is used when this is not set.
    pub cwd: Option<PathBuf>,
    /// Record the execution in RTK's tracking database.
    ///
    /// This defaults to false so embedding RTK has no unexpected persistence
    /// side effects.
    pub tracking: bool,
}

/// How RTK handled a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRoute {
    /// A built-in RTK filter executed the native command directly.
    Filtered { tool: &'static str },
    /// RTK executed a supported native command directly but preserved its raw
    /// output, such as when the command itself failed.
    DirectPassthrough { tool: &'static str },
    /// The original command was executed unchanged by the platform shell.
    ShellPassthrough,
}

/// Captured output and status from an embedded command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub route: ExecutionRoute,
}

impl ExecutionResult {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    pub fn filtered(&self) -> bool {
        matches!(self.route, ExecutionRoute::Filtered { .. })
    }
}

/// Execute a shell command, applying an RTK filter when it can be routed
/// without changing shell semantics.
///
/// Unsupported commands and complex shell syntax are passed to the platform
/// shell unchanged. The function captures output and never prints or exits the
/// embedding process.
pub fn execute(command: &str) -> Result<ExecutionResult> {
    execute_with_options(command, &ExecuteOptions::default())
}

/// Execute a shell command with explicit embedding options.
pub fn execute_with_options(command: &str, options: &ExecuteOptions) -> Result<ExecutionResult> {
    let cwd = match &options.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().context("Failed to determine command working directory")?,
    };

    if let Some(words) = plain_words(command) {
        if let Some(result) = execute_filtered(&words, &cwd, options.tracking)? {
            return Ok(result);
        }
    }

    execute_shell(command, &cwd)
}

fn execute_filtered(
    words: &[String],
    cwd: &Path,
    tracking: bool,
) -> Result<Option<ExecutionResult>> {
    let Some((tool, args)) = words.split_first() else {
        return Ok(None);
    };

    match tool.as_str() {
        // A bare `wc` reads stdin. The embedded API does not accept stdin yet,
        // so leave that case to the shell executor, whose stdin is null.
        "wc" if has_file_operand(args) => {
            let captured = wc_cmd::capture(args, cwd, tracking)
                .context("Failed to execute filtered wc command")?;
            Ok(Some(ExecutionResult {
                stdout: captured.stdout,
                stderr: captured.stderr,
                exit_code: captured.exit_code,
                route: if captured.filtered {
                    ExecutionRoute::Filtered { tool: "wc" }
                } else {
                    ExecutionRoute::DirectPassthrough { tool: "wc" }
                },
            }))
        }
        _ => Ok(None),
    }
}

fn has_file_operand(args: &[String]) -> bool {
    let mut after_separator = false;
    args.iter().any(|arg| {
        if after_separator {
            return true;
        }
        if arg == "--" {
            after_separator = true;
            return false;
        }
        !arg.starts_with('-')
    })
}

/// Return words only for syntax whose argv can be reconstructed without a
/// shell. Quoting and escaping are intentionally rejected in this first API
/// slice; false negatives merely use the behavior-preserving shell fallback.
fn plain_words(command: &str) -> Option<Vec<String>> {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|character| matches!(character, '\'' | '"' | '\\' | '\n' | '\r'))
    {
        return None;
    }

    let tokens = shell_lexer::tokenize(trimmed);
    if tokens.is_empty() || tokens.iter().any(|token| token.kind != TokenKind::Arg) {
        return None;
    }

    Some(tokens.into_iter().map(|token| token.value).collect())
}

fn execute_shell(command: &str, cwd: &Path) -> Result<ExecutionResult> {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let output = Command::new(shell)
        .arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Failed to execute command through {shell}"))?;

    Ok(ExecutionResult {
        stdout: decode_process_output(&output.stdout),
        stderr: decode_process_output(&output.stderr),
        exit_code: status_to_exit_code(output.status),
        route: ExecutionRoute::ShellPassthrough,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn filters_plain_wc_command_without_printing() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("lines.txt"), "alpha\nbeta\n").expect("write fixture");
        let options = ExecuteOptions {
            cwd: Some(temp.path().to_path_buf()),
            tracking: false,
        };

        let result = execute_with_options("wc -l lines.txt", &options).expect("execute wc");

        assert_eq!(result.stdout.trim(), "2");
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.route, ExecutionRoute::Filtered { tool: "wc" });
    }

    #[test]
    fn failed_filtered_command_returns_raw_failure_without_exiting() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let options = ExecuteOptions {
            cwd: Some(temp.path().to_path_buf()),
            tracking: false,
        };

        let result =
            execute_with_options("wc -l missing.txt", &options).expect("execute failing wc");

        assert_ne!(result.exit_code, 0);
        assert!(result.stderr.contains("missing.txt"));
        assert_eq!(
            result.route,
            ExecutionRoute::DirectPassthrough { tool: "wc" }
        );
    }

    #[test]
    fn complex_shell_command_falls_back_unchanged() {
        let result = execute("printf 'alpha\\nbeta\\n' | wc -l").expect("execute pipeline");

        assert_eq!(result.stdout.trim(), "2");
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.route, ExecutionRoute::ShellPassthrough);
    }

    #[test]
    fn unsupported_command_falls_back_to_shell() {
        let result = execute("printf fallback").expect("execute unsupported command");

        assert_eq!(result.stdout, "fallback");
        assert_eq!(result.route, ExecutionRoute::ShellPassthrough);
    }

    #[test]
    fn nonzero_status_is_returned_without_exiting_host() {
        #[cfg(windows)]
        let command = "exit /B 7";
        #[cfg(not(windows))]
        let command = "exit 7";

        let result = execute(command).expect("execute failing command");

        assert_eq!(result.exit_code, 7);
        assert!(!result.success());
    }
}
