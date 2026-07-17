//! `rspacefs-pvc` — PVC primitives over rspacefs `LayerFS`.
//!
//! Three primitives that the FUSE daemon (`rspacefs-mount`), the CLI
//! (`rspacefs pvc ...`), `qregistry`, and `rspaced` all need:
//!
//! - **[`PvcMount`]** wraps a `LayerFS` with PVC-shaped lifecycle —
//!   empty lower allowed, owner uid/gid for the mount, upper that can
//!   be tmpfs or disk.
//! - **[`pivot_upper`]** atomically swaps the upper layer of a live
//!   `PvcMount` from one backing to another (e.g. tmpfs → disk). Used
//!   by `rspaced` to promote an "ephemeral-then-persistent" PVC after
//!   boot stabilises.
//! - **[`capture_layer`]** snapshots the current upper into a
//!   deterministic tar+zstd blob plus a sha256 digest, ready to push as
//!   a new PVC registry artifact. Used by `qregistry` to ingest
//!   workload-generated initial content as a new revision.
//!
//! No FUSE. No HTTP. No async runtime. Just functions over data
//! structures. The downstream binaries provide whatever transport
//! they need on top.
//!
//! See `../../enhancements/pvc-registry-content.md` for the full design
//! and the cross-project contracts.

pub mod blob;
pub mod capture;
pub mod mount;
pub mod pivot;
mod swap;

pub use blob::{apply_blob, ApplyReport};
pub use capture::{capture_layer, CaptureOptions, CaptureReport};
pub use mount::{PvcAccessMode, PvcLifecycle, PvcMount, PvcOptions};
pub use pivot::{pivot_upper, PivotReport};

use thiserror::Error;

/// Errors from this crate.
#[derive(Debug, Error)]
pub enum PvcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("vfs: {0}")]
    Vfs(#[from] vfs::VfsError),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("digest mismatch: expected {expected}, got {got}")]
    DigestMismatch { expected: String, got: String },
}
