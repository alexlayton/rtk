//! Controlled child-process execution for embedded RTK callers.

use command_group::{CommandGroup, GroupChild};
use std::fmt;
use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Runtime-neutral cancellation signal for an embedded command execution.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessControl {
    deadline: Option<Instant>,
    cancellation: Option<CancellationToken>,
}

impl ProcessControl {
    pub fn new(timeout: Option<Duration>, cancellation: Option<CancellationToken>) -> Self {
        let started = Instant::now();
        Self {
            deadline: timeout.and_then(|duration| started.checked_add(duration)),
            cancellation,
        }
    }

    fn interruption(&self) -> Option<Interruption> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Some(Interruption::Cancelled);
        }
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            .then_some(Interruption::TimedOut)
    }

    fn sleep_duration(&self) -> Duration {
        self.deadline.map_or(STATUS_POLL_INTERVAL, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .min(STATUS_POLL_INTERVAL)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interruption {
    Cancelled,
    TimedOut,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

#[derive(Debug)]
pub enum ProcessError {
    Interrupted {
        reason: Interruption,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Spawn(io::Error),
    Wait(io::Error),
    Terminate(io::Error),
    Read(io::Error),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted {
                reason: Interruption::Cancelled,
                ..
            } => formatter.write_str("command was cancelled"),
            Self::Interrupted {
                reason: Interruption::TimedOut,
                ..
            } => formatter.write_str("command timed out"),
            Self::Spawn(error) => write!(formatter, "failed to start command: {error}"),
            Self::Wait(error) => write!(formatter, "failed while waiting for command: {error}"),
            Self::Terminate(error) => write!(formatter, "failed to terminate command: {error}"),
            Self::Read(error) => write!(formatter, "failed to capture command output: {error}"),
        }
    }
}

impl std::error::Error for ProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Interrupted { .. } => None,
            Self::Spawn(error) | Self::Wait(error) | Self::Terminate(error) | Self::Read(error) => {
                Some(error)
            }
        }
    }
}

struct GroupGuard {
    child: Option<GroupChild>,
}

impl GroupGuard {
    fn new(child: GroupChild) -> Self {
        Self { child: Some(child) }
    }

    fn child(&mut self) -> &mut GroupChild {
        self.child
            .as_mut()
            .expect("group guard always owns a child until disarmed")
    }

    fn disarm(&mut self) {
        self.child.take();
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn a command in a process group/job, capture both output streams, and
/// terminate the complete group when timeout or cancellation is observed.
pub fn capture_command(
    command: &mut Command,
    control: &ProcessControl,
) -> Result<ProcessOutput, ProcessError> {
    if let Some(reason) = control.interruption() {
        return Err(ProcessError::Interrupted {
            reason,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.group_spawn().map_err(ProcessError::Spawn)?;
    let mut guard = GroupGuard::new(child);

    let stdout = guard
        .child()
        .inner()
        .stdout
        .take()
        .ok_or_else(|| ProcessError::Read(io::Error::other("missing child stdout pipe")))?;
    let stderr = guard
        .child()
        .inner()
        .stderr
        .take()
        .ok_or_else(|| ProcessError::Read(io::Error::other("missing child stderr pipe")))?;
    let stdout_reader = spawn_reader(stdout);
    let stderr_reader = spawn_reader(stderr);

    loop {
        let status = guard.child().try_wait().map_err(ProcessError::Wait)?;
        if let Some(status) = status {
            let (stdout, stderr) = collect_output(stdout_reader, stderr_reader)?;
            guard.disarm();
            return Ok(ProcessOutput {
                stdout,
                stderr,
                status,
            });
        }

        if let Some(reason) = control.interruption() {
            // Close the race between the status check and interruption check.
            if let Some(status) = guard.child().try_wait().map_err(ProcessError::Wait)? {
                let (stdout, stderr) = collect_output(stdout_reader, stderr_reader)?;
                guard.disarm();
                return Ok(ProcessOutput {
                    stdout,
                    stderr,
                    status,
                });
            }

            guard.child().kill().map_err(ProcessError::Terminate)?;
            guard.child().wait().map_err(ProcessError::Wait)?;
            let (stdout, stderr) = collect_output(stdout_reader, stderr_reader)?;
            guard.disarm();
            return Err(ProcessError::Interrupted {
                reason,
                stdout,
                stderr,
            });
        }

        thread::sleep(control.sleep_duration());
    }
}

fn spawn_reader(mut reader: impl Read + Send + 'static) -> JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output)?;
        Ok(output)
    })
}

fn collect_output(
    stdout_reader: JoinHandle<io::Result<Vec<u8>>>,
    stderr_reader: JoinHandle<io::Result<Vec<u8>>>,
) -> Result<(Vec<u8>, Vec<u8>), ProcessError> {
    let stdout = stdout_reader
        .join()
        .map_err(|_| ProcessError::Read(io::Error::other("stdout reader thread panicked")))?
        .map_err(ProcessError::Read)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| ProcessError::Read(io::Error::other("stderr reader thread panicked")))?
        .map_err(ProcessError::Read)?;
    Ok((stdout, stderr))
}
