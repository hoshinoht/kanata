//! Provider-neutral request/response IR, error kinds and the `Adapter` contract.

#![forbid(unsafe_code)]
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

#[macro_use]
mod macros;

pub mod adapter;
mod contracts;
mod embeddings;
mod images;
pub mod sanitize;
mod speech;
pub mod sse;

pub use contracts::*;
pub use embeddings::*;
pub use images::*;
pub use speech::*;
