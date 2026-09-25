//! sanduk: run a coding agent in a disposable container, with the API key held on the host.
//!
//! The container engines are [`sanduk_container`]; the per-process sandbox is
//! [`sanduk_sandbox`](https://docs.rs/sanduk-sandbox). This crate holds what only sanduk uses: the
//! providers, the relay, the agents, recipes and kits, and the CLI.

pub mod agent;
pub mod assistants;
pub mod catalog;
pub mod cli;
pub mod error;
pub mod http;
pub mod kits;
pub mod preflight;
pub mod providers;
pub mod recipes;
pub mod relay;
pub mod resources;
pub mod runs;
pub mod sections;
pub mod util;
