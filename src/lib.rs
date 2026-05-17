//! Linear subject backend plugin for Animus.
//!
//! This crate is consumed by `src/main.rs` (the stdio plugin binary) and by
//! `tests/contract.rs`. It exposes:
//!
//! - [`config::LinearConfig`] - environment-driven configuration
//! - [`client::LinearClient`] - thin reqwest wrapper around Linear's GraphQL API
//! - [`backend::LinearBackend`] - the `SubjectBackend` implementation
//! - [`status_map`] - bidirectional native <-> normalized status mapping

pub mod backend;
pub mod client;
pub mod config;
pub mod status_map;
