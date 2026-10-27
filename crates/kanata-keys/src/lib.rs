//! Application key storage: the host CLI, the locked store and usage state; no network path.

#![forbid(unsafe_code)]
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

use kanata_config as config;
use kanata_core as core;

pub use kanata_config::keys::{file, time};

pub mod cli;
pub mod quota;
pub mod store;
pub mod usage;
