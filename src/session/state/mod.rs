//! Persisted recent and hidden project state.
//!
//! `model` holds the serialized state and its transitions (`touch`, hiding),
//! `store` owns the atomic JSON file. The file lives under `state_dir` so the
//! picker can restore the recent list and hidden projects across runs.

mod model;
mod store;

pub use model::{PersistedState, RecentEntry};
pub use store::StateStore;
