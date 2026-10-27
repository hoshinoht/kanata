//! Concrete provider adapters and the outbound HTTP transport.

#![forbid(unsafe_code)]
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

use kanata_config as config;
use kanata_core as core;

pub use kanata_core::adapter::{Adapter, AdapterFuture, AdapterOutput, EventStream};
pub(crate) use kanata_core::adapter::{
    internal_error, invalid_request, status_error, unsupported_operation, upstream_failure,
};

pub mod anthropic;
mod chat_completions;
pub mod chatgpt;
pub(crate) mod diagnostics;
pub mod ollama;
pub mod openrouter;
pub mod probe;
mod responses;
pub mod speech;
pub mod vllm;

pub(crate) mod transport;

#[cfg(test)]
mod test_support;
