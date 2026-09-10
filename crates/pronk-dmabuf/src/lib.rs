//! Native Linux synchronization for caller-owned shared GPU allocations.
//!
//! No capture authority, allocation policy, or transport lifecycle lives here.
//! A transport returning a buffer does not establish GPU completion. The owner
//! must also order its next access after the native dependencies.

mod reservation;
mod sync_file;

pub use reservation::{export_dependencies, import_completion, Access};
pub use sync_file::{Completion, SyncFile};
