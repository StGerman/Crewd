//! The part of crew that both binaries link: `crewd`, the daemon, and `crewctl`, the client
//! that must never touch its store (#45).
//!
//! The standing rule is that anything both binaries need lives here, and nothing else does. If
//! `crewctl` comes to need something the daemon owns, it moves down into this crate; it is never
//! re-described in the client, because a type described twice is a renamed field that renders
//! as a blank column instead of failing the build. The tell that the rule has stopped being
//! applied is a `pub` item here that `crewctl` never names.

pub mod api;
pub mod build;
pub mod client;
pub mod fmt;
pub mod render;
pub mod snapshot;

pub use build::build;
pub use snapshot::{
    DeliveryView, HaltReason, HaltedWorker, Phase, RateLimitPause, Row, RunRecord, Snapshot,
    TokenUsage,
};
