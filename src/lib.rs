#![forbid(unsafe_code)]
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

pub use kanata_adapters as adapter;
pub use kanata_config as config;
pub use kanata_core as core;
use kanata_server::telemetry;
pub use kanata_server::{auth, routing, server};

/// Key storage, plus the server-side keys-file hot reload.
pub mod keys {
    pub use kanata_keys::*;
    pub use kanata_server::key_reload as reload;
}

pub mod cli;
mod diagnostics;
mod portal;
mod serve;

pub const NAME: &str = env!("CARGO_PKG_NAME");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
