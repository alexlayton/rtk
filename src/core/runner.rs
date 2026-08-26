//! Shared command execution skeleton for filter modules.

use anyhow::{Context, Result};
use std::process::Command;

use crate::core::stream::{self, FilterMode, StdinMode, StreamFilter};
use crate::core::tracking;

/// Compose `filtered` with an optional recovery `hint`, cap the total at `raw`
/// (never emit more tokens than the command), print it, and return what was
/// emitted so the caller tracks exactly that.
pub fn emit_guarded(filtered: &str, hint: Option<&str>, raw: &str) -> String {
    let body = match hint {
        Some(h) => format!("{}\n{}", filtered, h),
        None => filtered.to_string(),
    };
    let shown = crate::core::guard::never_worse(raw, &body).to_string();
    println!("{}", shown);
    shown
}

pub fn print_with_hint(
    filtered: &str,
    tee_raw: &str,
    guard_raw: &str,
    tee_label: &str,
    exit_code: i32,
) -> String {
    let hint = crate::core::tee::tee_and_hint(tee_raw, tee_label, exit_code);
    emit_guarded(filtered, hint.as_deref(), guard_raw)
}

#[derive(Default)]
pub struct RunOptions<'a> {
    pub tee_label: Option<&'a str>,
    pub filter_stdout_only: bool,
    pub skip_filter_on_failure: bool,
    pub no_trailing_newline: bool,
    /// Forward rtk's own stdin to the child process. Needed for commands that
    /// can read from a pipe (e.g. `cat file | rtk wc`); without it the child
    /// gets an empty stdin and reports zero.
    pub inherit_stdin: bool,
}

impl<'a> RunOptions<'a> {
    pub fn with_tee(label: &'a str) -> Self {
        Self {
            tee_label: Some(label),
            ..Default::default()
        }
    }

    pub fn stdout_only() -> Self {
        Self {
            filter_stdout_only: true,
            ..Default::default()
        }
    }

    pub fn tee(mut self, label: &'a str) -> Self {
        self.tee_label = Some(label);
        self
    }

    pub fn early_exit_on_failure(mut self) -> Self {
        self.skip_filter_on_failure = true;
        self
    }

    pub fn no_trailing_newline(mut self) -> Self {
        self.no_trailing_newline = true;
        self
    }

    pub fn inherit_stdin(mut self) -> Self {
        self.inherit_stdin = true;
        self
    }
}

pub type CaptureFilter<'a> = Box<dyn Fn(&str) -> String + 'a>;
pub type ExitAwareCaptureFilter<'a> = Box<dyn Fn(&str, i32) -> String + 'a>;

pub enum RunMode<'a> {
    Filtered(CaptureFilter<'a>),
    FilteredWithExit(ExitAwareCaptureFilter<'a>),
    Streamed(Box<dyn StreamFilter + 'a>),
    Passthrough,
}

/// Captured result of a command after RTK filtering.
///
/// Unlike the CLI runner, this value does not write to the embedding process's
/// stdout or stderr and is therefore safe to return from the library API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub filtered: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

fn capture_filtered<F>(
    mut cmd: Command,
    tool_name: &str,
    cmd_label: &str,
    filter_fn: F,
    opts: &RunOptions<'_>,
    track: bool,
    control: Option<&crate::core::process::ProcessControl>,
) -> Result<CapturedRun>
where
    F: Fn(&str, i32) -> String,
{
    let timer = tracking::TimedExecution::start();
    let stdin_mode = if opts.inherit_stdin {
        StdinMode::Inherit
    } else {
        StdinMode::Null
    };
    let result = match control {
        Some(control) => stream::run_capture_controlled(&mut cmd, control)
            .map_err(anyhow::Error::new)
            .with_context(|| format!("Failed to run {}", tool_name))?,
        None => stream::run_streaming(&mut cmd, stdin_mode, FilterMode::CaptureOnly)
            .with_context(|| format!("Failed to run {}", tool_name))?,
    };

    let exit_code = result.exit_code;
    let raw = &result.raw;
    let raw_stdout = &result.raw_stdout;

    if opts.skip_filter_on_failure && exit_code != 0 {
        if track {
            timer.track(cmd_label, &format!("rtk {}", cmd_label), raw, raw);
        }
        return Ok(CapturedRun {
            stdout: result.raw_stdout,
            stderr: result.raw_stderr,
            exit_code,
            filtered: false,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
        });
    }

    let text_to_filter = if opts.filter_stdout_only {
        raw_stdout
    } else {
        raw
    };
    let filtered = filter_fn(text_to_filter, exit_code);

    let raw_for_tracking = if opts.filter_stdout_only {
        raw_stdout
    } else {
        raw
    };

    let shown = if let Some(label) = opts.tee_label {
        let hint = crate::core::tee::tee_and_hint(raw, label, exit_code);
        let body = match hint {
            Some(hint) => format!("{}\n{}", filtered, hint),
            None => filtered,
        };
        crate::core::guard::never_worse(raw_for_tracking, &body).to_string()
    } else {
        crate::core::guard::never_worse(raw_for_tracking, &filtered).to_string()
    };

    if track {
        timer.track(
            cmd_label,
            &format!("rtk {}", cmd_label),
            raw_for_tracking,
            &shown,
        );
    }
    Ok(CapturedRun {
        stdout: shown,
        stderr: result.raw_stderr,
        exit_code,
        filtered: true,
        stdout_truncated: result.stdout_truncated,
        stderr_truncated: result.stderr_truncated,
    })
}

fn print_captured(result: &CapturedRun, opts: &RunOptions<'_>) {
    // Preserve the historical failure passthrough exactly: raw streams already
    // contain their own line endings and empty stdout emits nothing.
    if !result.filtered {
        if !result.stdout.trim().is_empty() {
            print!("{}", result.stdout);
        }
        if !result.stderr.trim().is_empty() {
            eprint!("{}", result.stderr);
        }
        return;
    }

    if opts.no_trailing_newline {
        print!("{}", result.stdout);
    } else {
        println!("{}", result.stdout);
    }
}

fn run_captured_filter<F>(
    cmd: Command,
    tool_name: &str,
    cmd_label: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
) -> Result<i32>
where
    F: Fn(&str, i32) -> String,
{
    let result = capture_filtered(cmd, tool_name, cmd_label, filter_fn, &opts, true, None)?;
    print_captured(&result, &opts);
    Ok(result.exit_code)
}

pub fn run(
    mut cmd: Command,
    tool_name: &str,
    args_display: &str,
    mode: RunMode<'_>,
    opts: RunOptions<'_>,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let cmd_label = format!("{} {}", tool_name, args_display);

    match mode {
        RunMode::Filtered(filter_fn) => run_captured_filter(
            cmd,
            tool_name,
            &cmd_label,
            move |text, _| filter_fn(text),
            opts,
        ),
        RunMode::FilteredWithExit(filter_fn) => run_captured_filter(
            cmd,
            tool_name,
            &cmd_label,
            move |text, exit_code| filter_fn(text, exit_code),
            opts,
        ),
        RunMode::Streamed(filter) => {
            let result =
                stream::run_streaming(&mut cmd, StdinMode::Null, FilterMode::Streaming(filter))
                    .with_context(|| format!("Failed to run {}", tool_name))?;

            if let Some(label) = opts.tee_label {
                if let Some(hint) =
                    crate::core::tee::tee_and_hint(&result.raw, label, result.exit_code)
                {
                    println!("{}", hint);
                }
            }

            timer.track(
                &cmd_label,
                &format!("rtk {}", cmd_label),
                &result.raw,
                &result.filtered,
            );
            Ok(result.exit_code)
        }
        RunMode::Passthrough => {
            let result =
                stream::run_streaming(&mut cmd, StdinMode::Inherit, FilterMode::Passthrough)
                    .with_context(|| format!("Failed to run {}", tool_name))?;

            timer.track_passthrough(&cmd_label, &format!("rtk {} (passthrough)", cmd_label));
            Ok(result.exit_code)
        }
    }
}

/// Execute and filter a command without writing to process-global output.
///
/// Tracking is opt-in for embedded callers so using RTK as a library does not
/// unexpectedly create or modify the user's RTK history database.
// The package currently compiles separate binary and library module graphs;
// this entry point is used by the library graph while remaining unused in the
// legacy binary graph until CLI dispatch moves behind the library boundary.
#[allow(dead_code)]
pub fn run_filtered_capture<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
    track: bool,
) -> Result<CapturedRun>
where
    F: Fn(&str) -> String,
{
    let cmd_label = format!("{} {}", tool_name, args_display);
    capture_filtered(
        cmd,
        tool_name,
        &cmd_label,
        move |text, _| filter_fn(text),
        &opts,
        track,
        None,
    )
}

/// Execute and filter without printing, with timeout and cancellation control.
#[allow(dead_code)]
pub fn run_filtered_capture_controlled<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
    track: bool,
    control: &crate::core::process::ProcessControl,
) -> Result<CapturedRun>
where
    F: Fn(&str) -> String,
{
    let cmd_label = format!("{} {}", tool_name, args_display);
    capture_filtered(
        cmd,
        tool_name,
        &cmd_label,
        move |text, _| filter_fn(text),
        &opts,
        track,
        Some(control),
    )
}

/// Exit-aware variant of [`run_filtered_capture_controlled`].
#[allow(dead_code)]
pub fn run_filtered_capture_controlled_with_exit<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
    track: bool,
    control: &crate::core::process::ProcessControl,
) -> Result<CapturedRun>
where
    F: Fn(&str, i32) -> String,
{
    let cmd_label = format!("{} {}", tool_name, args_display);
    capture_filtered(
        cmd,
        tool_name,
        &cmd_label,
        filter_fn,
        &opts,
        track,
        Some(control),
    )
}

pub fn run_filtered<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
) -> Result<i32>
where
    F: Fn(&str) -> String,
{
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::Filtered(Box::new(filter_fn)),
        opts,
    )
}

pub fn run_filtered_with_exit<F>(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter_fn: F,
    opts: RunOptions<'_>,
) -> Result<i32>
where
    F: Fn(&str, i32) -> String,
{
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::FilteredWithExit(Box::new(filter_fn)),
        opts,
    )
}

pub fn run_passthrough(tool: &str, args: &[std::ffi::OsString], verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("{} passthrough: {:?}", tool, args);
    }
    let mut cmd = crate::core::utils::resolved_command(tool);
    cmd.args(args);
    let args_str = tracking::args_display(args);
    run(
        cmd,
        tool,
        &args_str,
        RunMode::Passthrough,
        RunOptions::default(),
    )
}

pub fn run_streamed(
    cmd: Command,
    tool_name: &str,
    args_display: &str,
    filter: Box<dyn StreamFilter + '_>,
    opts: RunOptions<'_>,
) -> Result<i32> {
    run(
        cmd,
        tool_name,
        args_display,
        RunMode::Streamed(filter),
        opts,
    )
}
