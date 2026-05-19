//! # rspacefs-verity
//!
//! Pure-Rust userspace implementation of dm-verity / fs-verity-style
//! Merkle hash tree verification. Builds a SHA-256 Merkle tree over 4 KB
//! blocks of file content and verifies each block against the tree on read.
//! Same algorithm as Linux dm-verity, fs-verity, and Android Verified Boot —
//! no kernel module required.
//!
//! ## What it provides
//!
//! - [`MerkleTree`] — build/verify a SHA-256 Merkle tree over arbitrary
//!   block-aligned data.
//! - [`LayerManifest`] / [`FileEntry`] — JSON-serializable manifest pinning
//!   the root hash and per-file block ranges of a layer.
//! - [`VerifiedBlockCache`] — lock-free bitset that remembers which blocks
//!   have been verified this session (~1 bit / 4 KB block).
//! - [`VerifiedLayerVfs`] / [`VerifiedFS`] — wrap any read-only
//!   `vfs::FileSystem` and verify every block on read.
//!
//! ## Quick start
//!
//! ```no_run
//! use rspacefs_verity::{VerifiedFS, OnFailure};
//! use vfs::{PhysicalFS, VfsPath};
//! use std::path::PathBuf;
//!
//! let layer = VfsPath::new(PhysicalFS::new(PathBuf::from("/var/lib/myapp/lower-base")));
//! let verified = VerifiedFS::build(layer, OnFailure::Reject).unwrap();
//!
//! // Now wrap it in a VfsPath and use it anywhere a vfs::FileSystem is expected.
//! let root: VfsPath = verified.into();
//! ```
//!
//! ## Pairing with the overlay
//!
//! `rspacefs-verity` is designed to be used as a tamper-evident lower layer
//! beneath the [`rspacefs-core`](https://docs.rs/rspacefs-core) overlay.
//! Wrap each read-only lower in `VerifiedFS::build(...)`, then hand the
//! resulting `VfsPath` to `LayerFS::new(upper, vec![verified_lower, ...])`.
//! The overlay handles merge/whiteout/copy-up; this crate handles integrity.

#![warn(missing_docs)]

mod verity;

pub use verity::{
    FileEntry, Hash256, LayerManifest, MerkleTree, OnFailure, VerifiedBlockCache, VerifiedFS,
    VerifiedLayerVfs, BLOCK_SIZE, HASH_SIZE,
};
