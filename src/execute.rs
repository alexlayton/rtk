//! Embeddable command execution with RTK filtering.
//!
//! The initial router deliberately handles only shell commands that can be
//! represented faithfully as a direct process invocation. Unsupported tools
//! and shell syntax are executed unchanged by the platform shell.

use crate::cargo_cmd::{self, CargoCommand};
pub use crate::core::process::CancellationToken;
use crate::core::process::{capture_command, Interruption, ProcessControl, ProcessError};
use crate::core::runner::CapturedRun;
use crate::core::stream::status_to_exit_code;
use crate::core::utils::decode_process_output;
use crate::git_cmd::{self, GitCommand};
use crate::shell_lexer::{self, TokenKind};
use crate::{test_runner, wc_cmd};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const DEFAULT_OUTPUT_LIMIT: usize = 10 * 1024 * 1024;

/// Options controlling an embedded command execution.
#[derive(Debug, Clone)]
pub struct ExecuteOptions {
    /// Directory in which the child command runs. The process's current
    /// directory is used when this is not set.
    pub cwd: Option<PathBuf>,
    /// Record the execution in RTK's tracking database.
    ///
    /// This defaults to false so embedding RTK has no unexpected persistence
    /// side effects.
    pub tracking: bool,
    /// Maximum wall-clock duration for the complete execution.
    pub timeout: Option<Duration>,
    /// Optional signal that allows another thread to cancel the execution.
    pub cancellation: Option<CancellationToken>,
    /// Maximum bytes retained independently for stdout and stderr. Additional
    /// bytes are drained and discarded so children cannot block on full pipes.
    pub output_limit: usize,
}

impl Default for ExecuteOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            tracking: false,
            timeout: None,
            cancellation: None,
            output_limit: DEFAULT_OUTPUT_LIMIT,
        }
    }
}

impl ExecuteOptions {
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_tracking(mut self, tracking: bool) -> Self {
        self.tracking = tracking;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn with_output_limit(mut self, output_limit: usize) -> Self {
        self.output_limit = output_limit;
        self
    }
}

#[derive(Debug)]
pub enum BeforeStartError {
    Cancelled,
    TimedOut,
    WorkingDirectory(io::Error),
}

#[derive(Debug)]
pub enum MayHaveStartedKind {
    Cancelled,
    TimedOut,
    Spawn(io::Error),
    Wait(io::Error),
    Terminate(io::Error),
    Capture(io::Error),
    Filter(String),
}

#[derive(Debug, Default)]
pub struct PartialOutput {
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug)]
pub struct MayHaveStartedError {
    pub kind: MayHaveStartedKind,
    pub partial_output: PartialOutput,
}

/// Execution failure classified by whether retrying could repeat side effects.
#[derive(Debug)]
pub enum ExecuteError {
    BeforeStart(BeforeStartError),
    MayHaveStarted(MayHaveStartedError),
}

impl ExecuteError {
    pub fn may_have_started(&self) -> bool {
        matches!(self, Self::MayHaveStarted(_))
    }
}

impl fmt::Display for ExecuteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeStart(error) => write!(formatter, "command did not start: {error}"),
            Self::MayHaveStarted(error) => write!(formatter, "command may have started: {error}"),
        }
    }
}

impl std::error::Error for ExecuteError {}

impl fmt::Display for BeforeStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("execution was cancelled"),
            Self::TimedOut => formatter.write_str("execution timed out"),
            Self::WorkingDirectory(error) => {
                write!(formatter, "failed to determine working directory: {error}")
            }
        }
    }
}

impl fmt::Display for MayHaveStartedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            MayHaveStartedKind::Cancelled => formatter.write_str("execution was cancelled"),
            MayHaveStartedKind::TimedOut => formatter.write_str("execution timed out"),
            MayHaveStartedKind::Spawn(error) => write!(formatter, "spawn failed: {error}"),
            MayHaveStartedKind::Wait(error) => write!(formatter, "wait failed: {error}"),
            MayHaveStartedKind::Terminate(error) => {
                write!(formatter, "process-tree termination failed: {error}")
            }
            MayHaveStartedKind::Capture(error) => write!(formatter, "capture failed: {error}"),
            MayHaveStartedKind::Filter(error) => write!(formatter, "filter failed: {error}"),
        }
    }
}

#[derive(Debug, Clone)]
enum FilterRoute {
    Wc,
    Git(GitCommand),
    Cargo(CargoCommand),
    Test,
}

#[derive(Debug, Clone, Copy)]
enum ChainOperator {
    And,
    Or,
    Sequence,
}

struct DirectPlan {
    commands: Vec<Vec<String>>,
    operators: Vec<ChainOperator>,
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
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
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
pub fn execute(command: &str) -> Result<ExecutionResult, ExecuteError> {
    execute_with_options(command, &ExecuteOptions::default())
}

/// Execute a shell command with explicit embedding options.
pub fn execute_with_options(
    command: &str,
    options: &ExecuteOptions,
) -> Result<ExecutionResult, ExecuteError> {
    let cwd = match &options.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir()
            .map_err(BeforeStartError::WorkingDirectory)
            .map_err(ExecuteError::BeforeStart)?,
    };

    let control = ProcessControl::new(
        options.timeout,
        options.cancellation.clone(),
        options.output_limit,
    );
    if let Some(plan) = parse_direct_plan(command) {
        let routes = plan
            .commands
            .iter()
            .map(|words| classify_filtered_route(words))
            .collect::<Option<Vec<_>>>();
        if let Some(routes) = routes {
            return execute_direct_plan(plan, routes, &cwd, options.tracking, &control);
        }
    }

    execute_shell(command, &cwd, &control)
}

fn classify_filtered_route(words: &[String]) -> Option<FilterRoute> {
    let (tool, args) = words.split_first()?;
    match tool.as_str() {
        "wc" if has_file_operand(args) => Some(FilterRoute::Wc),
        "git" => match args.first().map(String::as_str) {
            Some("status") => Some(FilterRoute::Git(GitCommand::Status)),
            Some("diff") => Some(FilterRoute::Git(GitCommand::Diff)),
            Some("log") => Some(FilterRoute::Git(GitCommand::Log)),
            Some("show") => Some(FilterRoute::Git(GitCommand::Show)),
            _ => None,
        },
        "cargo" => match args.first().map(String::as_str) {
            Some("build") => Some(FilterRoute::Cargo(CargoCommand::Build)),
            Some("check") => Some(FilterRoute::Cargo(CargoCommand::Check)),
            Some("test") => Some(FilterRoute::Cargo(CargoCommand::Test)),
            _ => None,
        },
        _ if is_common_test_runner(words) => Some(FilterRoute::Test),
        _ => None,
    }
}

fn execute_direct_plan(
    plan: DirectPlan,
    routes: Vec<FilterRoute>,
    cwd: &Path,
    tracking: bool,
    control: &ProcessControl,
) -> Result<ExecutionResult, ExecuteError> {
    if plan.commands.len() == 1 {
        return execute_filtered(routes[0].clone(), &plan.commands[0], cwd, tracking, control);
    }

    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut exit_code = 0;
    let mut executed_any = false;
    let mut all_filtered = true;
    let mut stdout_truncated = false;
    let mut stderr_truncated = false;

    for (index, (words, route)) in plan.commands.iter().zip(routes).enumerate() {
        if index > 0 && !should_run(plan.operators[index - 1], exit_code) {
            continue;
        }

        let result = match execute_filtered(route, words, cwd, tracking, control) {
            Ok(result) => result,
            Err(error) => {
                return Err(enrich_chain_error(
                    error,
                    executed_any,
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                ));
            }
        };
        append_chain_output(&mut stdout, &result.stdout);
        append_chain_output(&mut stderr, &result.stderr);
        exit_code = result.exit_code;
        executed_any = true;
        all_filtered &= result.filtered();
        stdout_truncated |= result.stdout_truncated;
        stderr_truncated |= result.stderr_truncated;
    }

    Ok(ExecutionResult {
        stdout,
        stderr,
        exit_code,
        route: if all_filtered {
            ExecutionRoute::Filtered { tool: "chain" }
        } else {
            ExecutionRoute::DirectPassthrough { tool: "chain" }
        },
        stdout_truncated,
        stderr_truncated,
    })
}

fn should_run(operator: ChainOperator, previous_exit_code: i32) -> bool {
    match operator {
        ChainOperator::And => previous_exit_code == 0,
        ChainOperator::Or => previous_exit_code != 0,
        ChainOperator::Sequence => true,
    }
}

fn append_chain_output(output: &mut String, next: &str) {
    if next.is_empty() {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(next);
}

fn enrich_chain_error(
    error: ExecuteError,
    executed_any: bool,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
) -> ExecuteError {
    if !executed_any {
        return error;
    }

    let mut error = match error {
        ExecuteError::BeforeStart(error) => MayHaveStartedError {
            kind: match error {
                BeforeStartError::Cancelled => MayHaveStartedKind::Cancelled,
                BeforeStartError::TimedOut => MayHaveStartedKind::TimedOut,
                BeforeStartError::WorkingDirectory(error) => {
                    MayHaveStartedKind::Filter(error.to_string())
                }
            },
            partial_output: PartialOutput::default(),
        },
        ExecuteError::MayHaveStarted(error) => error,
    };
    let mut combined_stdout = stdout;
    let mut combined_stderr = stderr;
    append_chain_output(&mut combined_stdout, &error.partial_output.stdout);
    append_chain_output(&mut combined_stderr, &error.partial_output.stderr);
    error.partial_output.stdout = combined_stdout;
    error.partial_output.stderr = combined_stderr;
    error.partial_output.stdout_truncated |= stdout_truncated;
    error.partial_output.stderr_truncated |= stderr_truncated;
    ExecuteError::MayHaveStarted(error)
}

fn execute_filtered(
    route: FilterRoute,
    words: &[String],
    cwd: &Path,
    tracking: bool,
    control: &ProcessControl,
) -> Result<ExecutionResult, ExecuteError> {
    let (tool, args) = words
        .split_first()
        .expect("preflight rejects empty direct commands");

    match route {
        // A bare `wc` reads stdin. The embedded API does not accept stdin yet,
        // so leave that case to the shell executor, whose stdin is null.
        FilterRoute::Wc => wc_cmd::capture(args, cwd, tracking, control)
            .map(|captured| execution_from_capture(captured, "wc"))
            .map_err(map_embedded_error),
        FilterRoute::Git(command) => {
            let command_args = &args[1..];
            git_cmd::capture(command, command_args, cwd, tracking, control)
                .map(|captured| execution_from_capture(captured, "git"))
                .map_err(map_embedded_error)
        }
        FilterRoute::Cargo(command) => {
            let command_args = &args[1..];
            cargo_cmd::capture(command, command_args, cwd, tracking, control)
                .map(|captured| execution_from_capture(captured, "cargo"))
                .map_err(map_embedded_error)
        }
        FilterRoute::Test => test_runner::capture_test(tool, args, cwd, tracking, control)
            .map(|captured| execution_from_capture(captured, "test"))
            .map_err(map_embedded_error),
    }
}

fn execution_from_capture(captured: CapturedRun, tool: &'static str) -> ExecutionResult {
    ExecutionResult {
        stdout: captured.stdout,
        stderr: captured.stderr,
        exit_code: captured.exit_code,
        stdout_truncated: captured.stdout_truncated,
        stderr_truncated: captured.stderr_truncated,
        route: if captured.filtered {
            ExecutionRoute::Filtered { tool }
        } else {
            ExecutionRoute::DirectPassthrough { tool }
        },
    }
}

fn is_common_test_runner(words: &[String]) -> bool {
    let Some(tool) = words
        .first()
        .and_then(|tool| Path::new(tool).file_name().and_then(|name| name.to_str()))
    else {
        return false;
    };
    if matches!(
        tool,
        "pytest" | "jest" | "vitest" | "rspec" | "phpunit" | "pest"
    ) {
        return true;
    }
    let subcommand = words.get(1).map(String::as_str);
    (tool == "go" && subcommand == Some("test"))
        || (matches!(tool, "npm" | "pnpm" | "yarn") && subcommand == Some("test"))
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

/// Parse only literal argv joined by `&&`, `||`, or `;`. Any syntax requiring
/// shell expansion rejects the complete plan so the untouched command can run
/// exactly once through the platform shell.
fn parse_direct_plan(command: &str) -> Option<DirectPlan> {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(|character| {
            matches!(
                character,
                '\'' | '"' | '\\' | '\n' | '\r' | '\0' | '$' | '#' | '[' | ']'
            )
        })
    {
        return None;
    }

    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut words = Vec::new();
    for token in shell_lexer::tokenize(trimmed) {
        match token.kind {
            TokenKind::Arg if !token.value.starts_with('~') => words.push(token.value),
            TokenKind::Operator => {
                if words.is_empty() {
                    return None;
                }
                let operator = match token.value.as_str() {
                    "&&" => ChainOperator::And,
                    "||" => ChainOperator::Or,
                    ";" => ChainOperator::Sequence,
                    _ => return None,
                };
                commands.push(std::mem::take(&mut words));
                operators.push(operator);
            }
            _ => return None,
        }
    }

    if words.is_empty() {
        if matches!(operators.last(), Some(ChainOperator::Sequence)) {
            operators.pop();
        } else {
            return None;
        }
    } else {
        commands.push(words);
    }
    (!commands.is_empty() && operators.len() + 1 == commands.len()).then_some(DirectPlan {
        commands,
        operators,
    })
}

fn execute_shell(
    command: &str,
    cwd: &Path,
    control: &ProcessControl,
) -> Result<ExecutionResult, ExecuteError> {
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (shell, flag) = ("sh", "-c");

    let mut shell_command = Command::new(shell);
    shell_command.arg(flag).arg(command).current_dir(cwd);
    let output = capture_command(&mut shell_command, control).map_err(map_process_error)?;

    Ok(ExecutionResult {
        stdout: decode_process_output(&output.stdout),
        stderr: decode_process_output(&output.stderr),
        exit_code: status_to_exit_code(output.status),
        route: ExecutionRoute::ShellPassthrough,
        stdout_truncated: output.stdout_truncated,
        stderr_truncated: output.stderr_truncated,
    })
}

fn map_embedded_error(error: anyhow::Error) -> ExecuteError {
    match error.downcast::<ProcessError>() {
        Ok(error) => map_process_error(error),
        Err(error) => may_have_started(MayHaveStartedKind::Filter(format!("{error:#}"))),
    }
}

fn map_process_error(error: ProcessError) -> ExecuteError {
    match error {
        ProcessError::Interrupted {
            reason,
            started,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => {
            let kind = match reason {
                Interruption::Cancelled => MayHaveStartedKind::Cancelled,
                Interruption::TimedOut => MayHaveStartedKind::TimedOut,
            };
            if !started {
                return ExecuteError::BeforeStart(match reason {
                    Interruption::Cancelled => BeforeStartError::Cancelled,
                    Interruption::TimedOut => BeforeStartError::TimedOut,
                });
            }
            ExecuteError::MayHaveStarted(MayHaveStartedError {
                kind,
                partial_output: PartialOutput {
                    stdout: decode_process_output(&stdout),
                    stderr: decode_process_output(&stderr),
                    stdout_truncated,
                    stderr_truncated,
                },
            })
        }
        ProcessError::Spawn(error) => may_have_started(MayHaveStartedKind::Spawn(error)),
        ProcessError::Wait(error) => may_have_started(MayHaveStartedKind::Wait(error)),
        ProcessError::Terminate(error) => may_have_started(MayHaveStartedKind::Terminate(error)),
        ProcessError::Read(error) => may_have_started(MayHaveStartedKind::Capture(error)),
    }
}

fn may_have_started(kind: MayHaveStartedKind) -> ExecuteError {
    ExecuteError::MayHaveStarted(MayHaveStartedError {
        kind,
        partial_output: PartialOutput::default(),
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
        let options = ExecuteOptions::default().with_cwd(temp.path());

        let result = execute_with_options("wc -l lines.txt", &options).expect("execute wc");

        assert_eq!(result.stdout.trim(), "2");
        assert!(result.stderr.is_empty());
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.route, ExecutionRoute::Filtered { tool: "wc" });
    }

    #[test]
    fn failed_filtered_command_returns_raw_failure_without_exiting() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let options = ExecuteOptions::default().with_cwd(temp.path());

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
    fn safe_chains_preserve_and_or_sequence_semantics() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("one.txt"), "one\n").expect("write one");
        fs::write(temp.path().join("two.txt"), "one\ntwo\n").expect("write two");
        let options = ExecuteOptions::default().with_cwd(temp.path());

        let and_result = execute_with_options("wc -l one.txt && wc -l two.txt", &options)
            .expect("execute and chain");
        assert_eq!(and_result.stdout, "1\n2");
        assert_eq!(and_result.exit_code, 0);
        assert_eq!(and_result.route, ExecutionRoute::Filtered { tool: "chain" });

        let or_result = execute_with_options("wc -l missing.txt || wc -l two.txt", &options)
            .expect("execute or chain");
        assert_eq!(or_result.stdout, "2");
        assert_eq!(or_result.exit_code, 0);
        assert!(or_result.stderr.contains("missing.txt"));

        let sequence_result = execute_with_options("wc -l one.txt;wc -l two.txt", &options)
            .expect("execute sequence chain");
        assert_eq!(sequence_result.stdout, "1\n2");
        assert_eq!(sequence_result.exit_code, 0);
    }

    #[test]
    fn unsupported_chain_is_preflighted_and_executed_once_by_shell() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::write(temp.path().join("one.txt"), "one\n").expect("write one");
        let options = ExecuteOptions::default().with_cwd(temp.path());

        let result =
            execute_with_options("wc -l one.txt && printf fallback > marker.txt", &options)
                .expect("execute shell fallback");

        assert_eq!(result.route, ExecutionRoute::ShellPassthrough);
        assert!(result.stdout.contains("one.txt"));
        assert_eq!(
            fs::read_to_string(temp.path().join("marker.txt")).expect("marker"),
            "fallback"
        );
    }

    #[test]
    fn complex_or_expanding_syntax_is_not_directly_planned() {
        for command in [
            "wc -l file | cat",
            "wc -l file > count",
            "wc -l $FILE",
            "wc -l *.txt",
            "echo $(wc -l file)",
            "for file in *.txt; do wc -l $file; done",
            "wc -l 'file name' && wc -l other",
        ] {
            assert!(parse_direct_plan(command).is_none(), "{command}");
        }
    }

    #[test]
    fn unsupported_command_falls_back_to_shell() {
        let result = execute("printf fallback").expect("execute unsupported command");

        assert_eq!(result.stdout, "fallback");
        assert_eq!(result.route, ExecutionRoute::ShellPassthrough);
    }

    #[test]
    fn filters_high_value_git_routes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        run_in(temp.path(), "git", &["init", "-q"]);
        run_in(
            temp.path(),
            "git",
            &["config", "user.email", "rtk@example.com"],
        );
        run_in(temp.path(), "git", &["config", "user.name", "RTK Test"]);
        fs::write(temp.path().join("file.txt"), "first\n").expect("write tracked file");
        run_in(temp.path(), "git", &["add", "file.txt"]);
        run_in(temp.path(), "git", &["commit", "-qm", "initial"]);
        fs::write(temp.path().join("file.txt"), "first\nsecond\n").expect("modify file");
        let options = ExecuteOptions::default().with_cwd(temp.path());

        for command in ["git status", "git diff", "git log -1", "git show HEAD"] {
            let result = execute_with_options(command, &options).expect("execute git route");
            assert_eq!(result.exit_code, 0, "{command}: {}", result.stderr);
            assert_eq!(
                result.route,
                ExecutionRoute::Filtered { tool: "git" },
                "{command}"
            );
        }
    }

    #[test]
    fn filters_cargo_build_check_and_test_routes() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::create_dir(temp.path().join("src")).expect("create src");
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='embedded-route-test'\nversion='0.1.0'\nedition='2021'\n",
        )
        .expect("write manifest");
        fs::write(
            temp.path().join("src/lib.rs"),
            "pub fn answer() -> u8 { 42 }\n#[test]\nfn works() { assert_eq!(answer(), 42); }\n",
        )
        .expect("write crate");
        let options = ExecuteOptions::default().with_cwd(temp.path());

        for command in ["cargo build", "cargo check", "cargo test"] {
            let result = execute_with_options(command, &options).expect("execute cargo route");
            assert_eq!(result.exit_code, 0, "{command}: {}", result.stderr);
            assert_eq!(
                result.route,
                ExecutionRoute::Filtered { tool: "cargo" },
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn filters_common_test_runner_route() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temporary directory");
        let runner = temp.path().join("pytest");
        fs::write(
            &runner,
            "#!/bin/sh\nprintf '================ 2 passed in 0.01s ================\\n'\n",
        )
        .expect("write runner");
        let mut permissions = fs::metadata(&runner)
            .expect("runner metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&runner, permissions).expect("make runner executable");

        let result = execute(runner.to_str().expect("utf8 path")).expect("execute test runner");

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.route, ExecutionRoute::Filtered { tool: "test" });
    }

    fn run_in(cwd: &Path, program: &str, args: &[&str]) {
        let status = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("start fixture command");
        assert!(status.success(), "fixture command failed: {program}");
    }

    #[cfg(unix)]
    #[test]
    fn capture_discards_output_beyond_configured_limit() {
        let options = ExecuteOptions::default().with_output_limit(256);

        let result = execute_with_options("yes 1234567890 | head -c 4096", &options)
            .expect("execute large-output command");

        assert_eq!(result.stdout.len(), 256);
        assert!(result.stdout_truncated);
        assert!(!result.stderr_truncated);
    }

    #[test]
    fn timeout_stops_a_running_shell_command() {
        let options = ExecuteOptions::default().with_timeout(Duration::from_millis(50));
        #[cfg(windows)]
        let command = "ping -n 6 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let command = "sleep 5";

        let error = execute_with_options(command, &options).expect_err("command should time out");

        assert!(matches!(
            error,
            ExecuteError::MayHaveStarted(MayHaveStartedError {
                kind: MayHaveStartedKind::TimedOut,
                ..
            })
        ));
    }

    #[test]
    fn cancellation_stops_a_running_shell_command() {
        let cancellation = CancellationToken::new();
        let options = ExecuteOptions::default().with_cancellation(cancellation.clone());
        #[cfg(windows)]
        let command = "ping -n 6 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let command = "sleep 5";

        let worker = std::thread::spawn(move || execute_with_options(command, &options));
        std::thread::sleep(Duration::from_millis(50));
        cancellation.cancel();
        let error = worker
            .join()
            .expect("execution thread")
            .expect_err("command should be cancelled");

        assert!(matches!(
            error,
            ExecuteError::MayHaveStarted(MayHaveStartedError {
                kind: MayHaveStartedKind::Cancelled,
                ..
            })
        ));
    }

    #[test]
    fn cancellation_before_spawn_is_safe_to_retry() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let options = ExecuteOptions::default().with_cancellation(cancellation);

        let error = execute_with_options("printf should-not-run", &options)
            .expect_err("pre-cancelled command");

        assert!(matches!(
            &error,
            ExecuteError::BeforeStart(BeforeStartError::Cancelled)
        ));
        assert!(!error.may_have_started());
    }

    #[cfg(unix)]
    #[test]
    fn interruption_returns_partial_output_without_retrying() {
        let options = ExecuteOptions::default().with_timeout(Duration::from_millis(50));

        let error = execute_with_options("printf started; sleep 5", &options)
            .expect_err("command should time out");

        let ExecuteError::MayHaveStarted(error) = error else {
            panic!("timeout after output must be classified as may-have-started");
        };
        assert!(matches!(error.kind, MayHaveStartedKind::TimedOut));
        assert_eq!(error.partial_output.stdout, "started");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_shell_descendants() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let options = ExecuteOptions::default()
            .with_cwd(temp.path())
            .with_timeout(Duration::from_millis(100));
        let command = "sh -c 'sleep 30 & echo $! > child.pid; wait'";

        let error = execute_with_options(command, &options).expect_err("command should time out");
        assert!(matches!(
            error,
            ExecuteError::MayHaveStarted(MayHaveStartedError {
                kind: MayHaveStartedKind::TimedOut,
                ..
            })
        ));
        let pid = fs::read_to_string(temp.path().join("child.pid"))
            .expect("child pid file")
            .trim()
            .to_string();

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline
            && Command::new("kill")
                .args(["-0", &pid])
                .status()
                .is_ok_and(|status| status.success())
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = Command::new("kill")
            .args(["-0", &pid])
            .status()
            .expect("probe descendant");
        assert!(
            !status.success(),
            "descendant process {pid} survived timeout"
        );
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
