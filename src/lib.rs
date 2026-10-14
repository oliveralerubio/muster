//! Muster: sync machines, AI agent configuration and secrets.
#![forbid(unsafe_code)]

pub mod adapters;
pub mod cli;
pub mod commands;
pub mod context;
pub mod doctor;
pub mod engine;
pub mod errors;
pub mod fleet;
pub mod git;
pub mod identity;
pub mod importers;
pub mod machine;
pub mod manifest;
pub mod output;
pub mod providers;
pub mod registry;
pub mod secrets;
pub mod vault;
