//! Build a Merkle tree over a small in-memory tree, serialize the
//! manifest, verify a clean read, then mutate a block and watch
//! verification fail. Run with:
//!
//!     cargo run --example verity_build

use std::io::{Read, Write};

use rspacefs_verity::{MerkleTree, OnFailure, VerifiedLayerVfs, BLOCK_SIZE};
use vfs::{MemoryFS, VfsPath};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build a small layer to verify.
    let root: VfsPath = MemoryFS::new().into();
    root.join("etc")?.create_dir_all()?;
    root.join("etc/release")?
        .create_file()?
        .write_all(b"v1.0\n")?;
    root.join("etc/hostname")?
        .create_file()?
        .write_all(b"shipyard\n")?;
    // A file that spans a couple of blocks (8 KB).
    let big = vec![0xCDu8; BLOCK_SIZE * 2];
    root.join("usr/lib/libtest.so")?.create_dir_all()?;
    root.join("usr/lib/libtest.so")?
        .create_file()?
        .write_all(&big)?;

    println!("=== build Merkle tree from the layer ===");
    let (tree, manifest) = MerkleTree::build_from_vfs(&root)?;
    println!("files       : {}", manifest.files.len());
    println!("block_size  : {}", manifest.block_size);
    let hex: String = manifest
        .root_hash
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    println!("root_hash   : {}", hex);

    println!("\n=== serialize manifest to JSON ===");
    let json = serde_json::to_string_pretty(&manifest)?;
    println!("{}", json.lines().take(8).collect::<Vec<_>>().join("\n"));
    println!("(…)");

    println!("\n=== verify a clean read ===");
    let root_hash = manifest.root_hash;
    let verified = VerifiedLayerVfs::new(
        root.clone(),
        tree.clone(),
        manifest.clone(),
        root_hash,
        OnFailure::Reject,
    );
    let (checked, failed) = verified.full_check()?;
    println!("full_check  : {} blocks checked, {} failed", checked, failed);

    println!("\n=== mutate a block and re-verify ===");
    {
        let mut f = root.join("etc/release")?.create_file()?;
        f.write_all(b"TAMPERED-WITH\n")?;
    }
    // Read the bytes back the way a consumer would.
    let mut buf = Vec::new();
    root.join("etc/release")?.open_file()?.read_to_end(&mut buf)?;
    // Verifying the (now-tampered) bytes against the original tree:
    match verified.verify_file_blocks("etc/release", &buf, 0) {
        Ok(_) => println!("verify_file_blocks: OK (BUG — should have failed!)"),
        Err(e) => println!("verify_file_blocks: rejected as expected: {}", e),
    }

    Ok(())
}
