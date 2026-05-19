//! Compose a verified read-only lower layer beneath a writable upper layer.
//!
//! Run with `cargo run --example verified_overlay`.
//!
//! Demonstrates the typical "container rootfs" use case: tamper-evident
//! base image (verity-verified) plus a writable workspace on top (overlay).

use std::io::{Read, Write};

use rspacefs_core::OverlayFS;
use rspacefs_verity::{OnFailure, VerifiedFS};
use vfs::{MemoryFS, VfsPath};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Build the "base image" — a read-only lower layer we'll verify.
    let base: VfsPath = MemoryFS::new().into();
    base.join("etc")?.create_dir()?;
    base.join("etc/release")?
        .create_file()?
        .write_all(b"rspacefs-base 1.0\n")?;
    base.join("usr/bin")?.create_dir_all()?;
    base.join("usr/bin/hello")?
        .create_file()?
        .write_all(b"#!/bin/sh\necho hello from base\n")?;

    // ── 2. Wrap the base in a VerifiedFS, pinning its current root hash.
    let verified = VerifiedFS::build(base, OnFailure::Reject)?;
    let root_hex: String = verified
        .root_hash()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    println!("verified base root hash: {}", root_hex);
    let verified_path: VfsPath = verified.into();

    // ── 3. Stack a writable upper layer on top.
    let upper: VfsPath = MemoryFS::new().into();
    let overlay: VfsPath = OverlayFS::new(upper, vec![verified_path]).into();

    // ── 4. Read through the merged view — verity verifies each block.
    let mut buf = String::new();
    overlay
        .join("etc/release")?
        .open_file()?
        .read_to_string(&mut buf)?;
    println!("/etc/release -> {}", buf.trim_end());

    // ── 5. Write to the merged view — goes to upper, base stays intact.
    overlay
        .join("etc/hostname")?
        .create_file()?
        .write_all(b"workshop\n")?;
    let mut buf = String::new();
    overlay
        .join("etc/hostname")?
        .open_file()?
        .read_to_string(&mut buf)?;
    println!("/etc/hostname -> {}", buf.trim_end());

    // ── 6. List the merged directory.
    print!("/etc contents:");
    for entry in overlay.join("etc")?.read_dir()? {
        print!(" {}", entry.filename());
    }
    println!();

    Ok(())
}
