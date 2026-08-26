//! Embeddable RTK command routing, execution, and output filtering.
//!
//! [`execute`] is the stable entry point for applications that want RTK's
//! filtering without requiring an installed `rtk` executable.

#[allow(dead_code)]
#[path = "cmds/rust/cargo_cmd.rs"]
mod cargo_cmd;
#[allow(dead_code)]
#[path = "library_core.rs"]
mod core;
mod execute;
#[allow(dead_code)]
#[path = "cmds/git/git.rs"]
mod git_cmd;
#[allow(dead_code)]
#[path = "discover/lexer.rs"]
mod shell_lexer;
#[allow(dead_code)]
#[path = "cmds/rust/runner.rs"]
mod test_runner;
#[allow(dead_code)]
#[path = "cmds/system/wc_cmd.rs"]
mod wc_cmd;

pub use core::{tracking, utils};
pub use execute::{
    execute, execute_with_options, BeforeStartError, CancellationToken, ExecuteError,
    ExecuteOptions, ExecutionResult, ExecutionRoute, MayHaveStartedError, MayHaveStartedKind,
    PartialOutput, DEFAULT_OUTPUT_LIMIT,
};
