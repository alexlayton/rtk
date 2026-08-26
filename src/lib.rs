//! Embeddable RTK command routing, execution, and output filtering.
//!
//! [`execute`] is the stable entry point for applications that want RTK's
//! filtering without requiring an installed `rtk` executable.

#[allow(dead_code)]
#[path = "library_core.rs"]
mod core;
mod execute;
#[allow(dead_code)]
#[path = "discover/lexer.rs"]
mod shell_lexer;
#[allow(dead_code)]
#[path = "cmds/system/wc_cmd.rs"]
mod wc_cmd;

pub use core::{tracking, utils};
pub use execute::{
    execute, execute_with_options, CancellationToken, ExecuteOptions, ExecutionResult,
    ExecutionRoute,
};
