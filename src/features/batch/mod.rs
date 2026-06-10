//! Batch processing feature
//!
//! OpenAI-style `/v1/batch/chat/completions` endpoint for asynchronous,
//! lower-priority processing of multiple chat completion requests.

pub mod api;
pub mod domain;
pub mod infra;

pub use domain::{BatchJob, BatchJobStatus, BatchRequestBody, BatchService};
pub use infra::{SqlxBatchRepository, spawn_batch_worker};
