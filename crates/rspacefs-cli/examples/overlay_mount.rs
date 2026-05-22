//! Build a layered view of three in-memory layers and demonstrate the
//! merge / whiteout / copy-up semantics. Run with:
//!
//!     cargo run --example overlay_mount

use std::io::{Read, Write};

use rspacefs_core::{LayerFS, OPAQUE_WHITEOUT, WHITEOUT_PREFIX};
use vfs::{MemoryFS, VfsPath};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Three layers, top-down priority order.
    let upper: VfsPath = MemoryFS::new().into();
    let app: VfsPath = MemoryFS::new().into();
    let base: VfsPath = MemoryFS::new().into();

    // Base layer: minimal OS-like tree.
    base.join("etc")?.create_dir()?;
    base.join("etc/hostname")?
        .create_file()?
        .write_all(b"base-host\n")?;
    base.join("etc/release")?
        .create_file()?
        .write_all(b"base 1.0\n")?;
    base.join("usr/bin")?.create_dir_all()?;
    base.join("usr/bin/sh")?
        .create_file()?
        .write_all(b"#!/bin/sh\n")?;

    // App layer: overrides /etc/release, adds /app.
    app.join("etc")?.create_dir()?;
    app.join("etc/release")?
        .create_file()?
        .write_all(b"app on base\n")?;
    app.join("app")?.create_dir()?;
    app.join("app/main.py")?
        .create_file()?
        .write_all(b"print('hi from app')\n")?;

    // Build the layered view: upper writable, app over base.
    let root: VfsPath = LayerFS::new(upper.clone(), vec![app, base]).into();

    println!("=== read merged content ===");
    let mut buf = String::new();
    root.join("etc/release")?
        .open_file()?
        .read_to_string(&mut buf)?;
    println!("/etc/release    -> {}", buf.trim_end());
    buf.clear();
    root.join("etc/hostname")?
        .open_file()?
        .read_to_string(&mut buf)?;
    println!("/etc/hostname   -> {}", buf.trim_end());

    println!("\n=== readdir merges layers ===");
    for name in root.read_dir()? {
        print!(" {}", name.filename());
    }
    println!();

    println!("\n=== write triggers copy-up into upper ===");
    root.join("etc/release")?
        .append_file()?
        .write_all(b"+ modified locally\n")?;
    buf.clear();
    root.join("etc/release")?
        .open_file()?
        .read_to_string(&mut buf)?;
    println!("/etc/release (after append)    -> {:?}", buf);
    println!(
        "upper has it now?              -> {}",
        upper.join("etc/release")?.exists()?
    );

    println!("\n=== delete creates a whiteout marker ===");
    root.join("etc/hostname")?.remove_file()?;
    println!(
        "/etc/hostname exists in view?  -> {}",
        root.join("etc/hostname")?.exists()?
    );
    let wh_name = format!("{}{}", WHITEOUT_PREFIX, "hostname");
    let upper_path = format!("etc/{}", wh_name);
    println!(
        "upper has .wh.hostname?        -> {}",
        upper.join(&upper_path)?.exists()?
    );
    println!("(opaque whiteout would be {})", OPAQUE_WHITEOUT);

    Ok(())
}
