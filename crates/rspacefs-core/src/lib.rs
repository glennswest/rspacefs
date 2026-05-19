//! # rspacefs-core
//!
//! Pure-Rust userspace layered virtual filesystem — a **replacement for
//! kernel overlayfs**, built on the [`vfs`](https://crates.io/crates/vfs)
//! trait. Merges multiple read-only lower layers with a writable upper
//! layer, with OCI-spec whiteout markers and copy-up-on-write semantics —
//! the same model the Linux kernel `fs/overlayfs/` driver uses,
//! implemented entirely in user-space.
//!
//! This crate is intentionally minimal: no async, no kernel module, no
//! networking, no protocol. Operations are direct synchronous calls
//! against whatever `vfs::FileSystem` backend you plug in (real disk via
//! `PhysicalFS`, in-memory via `MemoryFS`, your own custom backend, etc.).
//! Performance and concurrency are the caller's responsibility.
//!
//! ## Quick start
//!
//! ```no_run
//! use rspacefs_core::LayerFS;
//! use vfs::{PhysicalFS, VfsPath};
//! use std::path::PathBuf;
//!
//! let upper = VfsPath::new(PhysicalFS::new(PathBuf::from("/var/lib/myapp/upper")));
//! let base  = VfsPath::new(PhysicalFS::new(PathBuf::from("/var/lib/myapp/lower-base")));
//! let app   = VfsPath::new(PhysicalFS::new(PathBuf::from("/var/lib/myapp/lower-app")));
//!
//! // Lower layers are top-down: index 0 is highest priority.
//! let layers = LayerFS::new(upper, vec![app, base]);
//! let root: VfsPath = layers.into();
//!
//! // Now `root` behaves like a merged read/write filesystem.
//! ```
//!
//! ## OCI image semantics
//!
//! - Whiteouts use the OCI image-spec convention: `.wh.<name>` to delete a
//!   single entry, and `.wh..wh..opq` to mark an opaque directory (lower
//!   layers are not consulted past an opaque marker).
//! - Copy-up: the first write to a file that only exists in a lower layer
//!   triggers a full content copy into the upper layer before the write
//!   proceeds. Matches kernel overlayfs cost — measure before optimizing.
//!
//! ## See also
//!
//! - [`rspacefs-verity`](https://docs.rs/rspacefs-verity) — dm-verity-style
//!   Merkle-tree integrity verification, often combined with this crate
//!   for tamper-evident lower layers.

#![warn(missing_docs)]

mod layer;

pub use layer::{LayerFS, OPAQUE_WHITEOUT, WHITEOUT_PREFIX};
