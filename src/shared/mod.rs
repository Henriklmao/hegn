//! Cross-cutting helpers without domain dependencies.
//!
//! Everything in this module is deliberately small and free of higher-level
//! imports so any domain can use it without creating a dependency cycle.
//! Domain-specific behaviour stays in the domain modules.

pub mod fs;
pub mod math;
pub mod paths;
pub mod process;
pub mod text;
