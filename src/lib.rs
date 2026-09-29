//! crewd — a tracker-driven orchestrator for Claude Code agents.
//!
//! Layering follows the Symphony spec's better instincts: a deterministic coordination layer
//! that owns polling, claims, concurrency and retries, sitting above pluggable execution and
//! integration layers. Every external effect is a trait so the scheduler can be tested with
//! fakes, on a fake clock, with no sleeps and no tokens spent.

pub mod api;
pub mod broker;
pub mod clock;
pub mod config;
pub mod credentials;
pub mod forge;
pub mod gate;
pub mod http;
pub mod init;
pub mod model;
pub mod project;
pub mod sched;
pub mod store;
pub mod tracker;
pub mod transcript;
pub mod tui;
pub mod worker;
pub mod workspace;
