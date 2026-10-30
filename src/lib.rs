//! Muster: sync machines, AI agent configuration and secrets.
#![forbid(unsafe_code)]

pub mod adapters;
pub mod audit;
pub mod cli;
pub mod commands;
pub mod context;
pub mod doctor;
pub mod engine;
pub mod errors;
pub mod fleet;
pub mod git;
pub mod hash;
pub mod identity;
pub mod importers;
pub mod installer;
pub mod machine;
pub mod manifest;
pub mod mcp;
pub mod output;
pub mod providers;
pub mod registry;
pub mod secret_leases;
pub mod secrets;
pub mod skills;
pub mod team;
pub mod usage;
pub mod vault;
pub mod web;
