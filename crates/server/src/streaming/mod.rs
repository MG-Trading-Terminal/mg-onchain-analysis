//! Streaming detector subsystem.
//!
//! Modules:
//! - `registry`  — `StreamingRegistry` (which tokens are being streamed)
//! - `scheduler` — `DetectorScheduler` (debounce + queue drain)
//! - `worker`    — `SchedulerWorker` (detector evaluation + metrics)

pub mod registry;
pub mod scheduler;
pub mod worker;
