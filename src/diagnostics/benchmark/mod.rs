//! `hegn benchmark` — measured host cost of running instances.
//!
//! The report answers the questions behind `docs/resources.md`: how much RAM
//! the VMM, `virtiofsd` and the console relay actually use, how much disk the
//! images, snapshots and logs occupy, and what the configured guests would
//! cost at most.
//!
//! Three modes:
//!
//! - no argument: measure every running instance (read-only), or ask whether to
//!   boot a throwaway one when nothing is running;
//! - `benchmark <project>`: boot that project's instance (or reuse it) and
//!   measure it;
//! - `benchmark --clean`: create a temporary project, boot it, measure it, stop
//!   it and delete the project again.
//!
//! Process numbers come from `/proc/<pid>/status`, disk numbers from
//! `st_blocks` (allocated) and `st_size` (apparent), so sparse files such as
//! the cloud-init seed and the qcow2 image are reported honestly.

mod cleanup;
mod measure;
mod report;
mod usage;

pub use measure::{collect, run};
pub use report::{BenchmarkReport, Cleanup, DiskCost, ProcessCost, SessionCost, human_bytes};

const MIB: u64 = 1024 * 1024;
/// Prefix for throwaway projects created by `benchmark --clean`.
const TEMP_PREFIX: &str = "hegn-benchmark-";
