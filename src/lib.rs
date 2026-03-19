//! Core library for the `skillctl` CLI.
//!
//! The current bootstrap exposes a typed command runtime with explicit domain
//! modules so future CLI, MCP, and TUI work can share one execution model.
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod adapter;
pub mod app;
pub mod cli;
pub mod doctor;
pub mod error;
pub mod history;
pub mod lockfile;
pub mod manifest;
pub mod materialize;
pub mod mcp;
pub mod overlay;
pub mod planner;
pub mod resolver;
pub mod response;
pub mod runtime;
pub mod skill;
pub mod source;
pub mod state;
pub mod telemetry;
pub mod tui;

pub use runtime::{run, run_from_args};
