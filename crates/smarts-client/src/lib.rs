//! Rust client for the smarts.bio public API gateway (`bioinformatics-api`).
//!
//! The gateway is the contract: this crate is a thin REST + SSE layer over its
//! `/v1` surface, shared by the `smarts` CLI and (later) the MCP server.

mod client;
mod resources;

pub mod config;
pub mod credentials;
pub mod error;
pub mod models;

pub use client::SmartsClient;
pub use config::{resolve_path, Config, DEFAULT_BASE_URL};
pub use error::{Error, Result};
pub use resources::DIRECT_UPLOAD_LIMIT;
