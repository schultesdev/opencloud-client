//! Sync engine (Phase 5): three-way comparison against the status
//! database, initial full sync, selective-sync filters, local and remote
//! change detection, transfers with a parallel queue, and the job loop
//! with pause/resume/stop. Conflict resolution lands in Phase 6.
//!
//! Building blocks:
//!
//! - [`scan`]: the local directory scanner and the staged remote
//!   traversal (PROPFIND `Depth: 1` per folder, root-ETag shortcut).
//! - [`diff`]: the three-way comparison producing the action plan
//!   ([`SyncAction`]) of one sync pass.
//! - [`filter`]: the `include`/`exclude` selective-sync patterns.
//! - [`transfer`]: uploads with optimistic-write conditions, downloads
//!   via `.part` temp files (range-chunked for large files) with atomic
//!   renames and cross-platform read-only handling.
//! - [`watch`]: the debouncing filesystem watcher per job.
//! - [`conflict`]: conflict resolution (Phase 6) — `resolve_server`
//!   downloads over the local file, `resolve_local` uploads guarded by
//!   the conflict's ETag (a remote change in between keeps the conflict
//!   pending with fresh remote state), plus dated conflicted copies for
//!   the `conflict_copy` policy.
//! - [`engine`]: the job tasks — periodic and watcher-triggered sync
//!   passes, crash recovery, pause/resume/stop commands, the conflict
//!   policy, and the event stream for the UI (Phase 7).

pub mod conflict;
pub mod diff;
pub mod engine;
pub mod error;
pub mod filter;
pub mod progress;
pub mod scan;
pub mod transfer;
pub mod watch;

pub use conflict::{ResolveOutcome, resolve};
pub use diff::SyncAction;
pub use engine::{Engine, EngineEvent, EngineOptions, Job, JobCommand, RunSummary};
pub use error::SyncError;
pub use filter::Filter;
pub use progress::{RunProgress, TransferHandle};
pub use scan::{LocalNode, RemoteNode, RemoteSnapshot, ServerNames};
pub use transfer::{LocalGuard, TransferMeta, TransferPaths, TransferResult};
pub use watch::LocalWatcher;
