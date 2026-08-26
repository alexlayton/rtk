//! Minimal internal module graph used by the embeddable execution API.
//!
//! The CLI currently owns a much larger module graph. Keeping the library
//! graph narrow avoids exposing CLI-only hooks, analytics, and clap routing to
//! embedding applications while that code is incrementally separated.

#[path = "core/args_utils.rs"]
pub mod args_utils;
#[path = "core/config.rs"]
pub mod config;
#[path = "core/constants.rs"]
pub mod constants;
#[path = "core/guard.rs"]
pub mod guard;
#[path = "core/process.rs"]
pub mod process;
#[path = "core/runner.rs"]
pub mod runner;
#[path = "core/stream.rs"]
pub mod stream;
#[path = "core/tee.rs"]
pub mod tee;
#[path = "core/tracking.rs"]
pub mod tracking;
#[path = "core/truncate.rs"]
pub mod truncate;
#[path = "core/utils.rs"]
pub mod utils;
