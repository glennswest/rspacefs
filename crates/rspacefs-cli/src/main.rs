//! `rspacefs` — CLI for inspecting OverlayFS mounts and managing verity layer manifests.
//!
//! Two subcommands: `overlay` (merge layers and read through them) and
//! `verity` (build / verify / inspect Merkle-tree layer manifests).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rspacefs_core::OverlayFS;
use rspacefs_verity::{
    LayerManifest, MerkleTree, OnFailure, VerifiedLayerVfs, BLOCK_SIZE,
};
use vfs::{PhysicalFS, VfsPath};

#[derive(Parser)]
#[command(name = "rspacefs", version, about = "Userspace OverlayFS + verity tools", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Operate on a merged overlay (upper + N lower layers).
    Overlay {
        #[command(subcommand)]
        op: OverlayOp,
    },
    /// Build, verify, or inspect a Merkle-tree layer manifest.
    Verity {
        #[command(subcommand)]
        op: VerityOp,
    },
}

#[derive(Subcommand)]
enum OverlayOp {
    /// List the entries of a directory through the overlay (lowers merged into upper).
    Ls {
        /// Writable upper layer.
        #[arg(long)]
        upper: PathBuf,
        /// Read-only lower layers, repeat (`--lower DIR --lower DIR`). Order is top-down.
        #[arg(long)]
        lower: Vec<PathBuf>,
        /// Path inside the overlay (default: root).
        #[arg(default_value = "")]
        path: String,
    },
    /// Cat a file through the overlay.
    Cat {
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        lower: Vec<PathBuf>,
        path: String,
    },
    /// Print whether a path exists in the overlay, and which layer it resolves from.
    Stat {
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        lower: Vec<PathBuf>,
        path: String,
    },
}

#[derive(Subcommand)]
enum VerityOp {
    /// Build a Merkle tree over every file in a directory tree; emit a layer manifest.
    Build {
        /// Directory to scan.
        dir: PathBuf,
        /// Write the manifest JSON here (default: stdout).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Also write the binary Merkle tree to this file.
        #[arg(long)]
        tree: Option<PathBuf>,
    },
    /// Verify every block in a directory tree against an existing manifest.
    Verify {
        /// Directory to verify.
        dir: PathBuf,
        /// Manifest JSON to verify against.
        #[arg(long)]
        manifest: PathBuf,
        /// On failure, log a warning but continue (default: reject).
        #[arg(long)]
        warn_only: bool,
    },
    /// Pretty-print a manifest JSON file (root hash, file count, etc.).
    Inspect {
        manifest: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Overlay { op } => run_overlay(op),
        Cmd::Verity { op } => run_verity(op),
    }
}

// ── overlay subcommands ──────────────────────────────────────────────────────

fn build_overlay(upper: PathBuf, lower: Vec<PathBuf>) -> Result<VfsPath> {
    if !upper.exists() {
        bail!("upper layer does not exist: {}", upper.display());
    }
    for l in &lower {
        if !l.exists() {
            bail!("lower layer does not exist: {}", l.display());
        }
    }
    let upper_vfs = VfsPath::new(PhysicalFS::new(upper));
    let lower_vfs: Vec<VfsPath> = lower
        .into_iter()
        .map(|p| VfsPath::new(PhysicalFS::new(p)))
        .collect();
    Ok(VfsPath::new(OverlayFS::new(upper_vfs, lower_vfs)))
}

fn run_overlay(op: OverlayOp) -> Result<()> {
    match op {
        OverlayOp::Ls { upper, lower, path } => {
            let root = build_overlay(upper, lower)?;
            let target = if path.is_empty() {
                root
            } else {
                root.join(&path)?
            };
            let entries: Vec<String> = target
                .read_dir()
                .with_context(|| format!("reading directory {:?}", path))?
                .map(|e| e.filename())
                .collect();
            for name in entries {
                println!("{}", name);
            }
        }
        OverlayOp::Cat {
            upper,
            lower,
            path,
        } => {
            let root = build_overlay(upper, lower)?;
            let mut file = root.join(&path)?.open_file()?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            std::io::stdout().write_all(&buf)?;
        }
        OverlayOp::Stat {
            upper,
            lower,
            path,
        } => {
            let root = build_overlay(upper.clone(), lower.clone())?;
            let p = root.join(&path)?;
            if !p.exists()? {
                println!("not found in overlay: {}", path);
                return Ok(());
            }
            let meta = p.metadata()?;
            println!("type: {:?}", meta.file_type);
            println!("size: {}", meta.len);

            // Show which physical layer the entry resolves from, by checking
            // each layer in priority order.
            let abs = |base: &Path| -> PathBuf { base.join(path.trim_start_matches('/')) };
            let upper_path = abs(&upper);
            if upper_path.exists() {
                println!("layer: upper ({})", upper_path.display());
            } else {
                for (i, l) in lower.iter().enumerate() {
                    let lp = abs(l);
                    if lp.exists() {
                        println!("layer: lower[{}] ({})", i, lp.display());
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

// ── verity subcommands ──────────────────────────────────────────────────────

fn run_verity(op: VerityOp) -> Result<()> {
    match op {
        VerityOp::Build {
            dir,
            manifest,
            tree,
        } => {
            if !dir.exists() {
                bail!("directory does not exist: {}", dir.display());
            }
            let root = VfsPath::new(PhysicalFS::new(dir.clone()));
            let (merkle, mf) = MerkleTree::build_from_vfs(&root)
                .map_err(|e| anyhow!("building merkle tree: {e}"))?;

            let json = serde_json::to_string_pretty(&mf)?;
            match manifest {
                Some(p) => {
                    fs::write(&p, &json).with_context(|| format!("writing {}", p.display()))?;
                    eprintln!("wrote manifest to {}", p.display());
                }
                None => println!("{}", json),
            }

            if let Some(t) = tree {
                fs::write(&t, merkle.to_bytes())
                    .with_context(|| format!("writing tree to {}", t.display()))?;
                eprintln!("wrote tree to {} ({} nodes)", t.display(), merkle.node_count());
            }

            eprintln!(
                "root_hash: {}",
                mf.root_hash
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            );
            eprintln!("files: {}, block_size: {} bytes", mf.files.len(), BLOCK_SIZE);
        }
        VerityOp::Verify {
            dir,
            manifest,
            warn_only,
        } => {
            let mf_text = fs::read_to_string(&manifest)
                .with_context(|| format!("reading manifest {}", manifest.display()))?;
            let mf: LayerManifest = serde_json::from_str(&mf_text)?;

            let root = VfsPath::new(PhysicalFS::new(dir.clone()));
            let (rebuilt, current_mf) = MerkleTree::build_from_vfs(&root)
                .map_err(|e| anyhow!("scanning directory: {e}"))?;

            if rebuilt.root_hash() != mf.root_hash {
                eprintln!(
                    "expected root: {}",
                    mf.root_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>()
                );
                eprintln!(
                    "computed root: {}",
                    rebuilt
                        .root_hash()
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
                );
                if warn_only {
                    eprintln!("ROOT HASH MISMATCH (continuing because --warn-only)");
                } else {
                    bail!("ROOT HASH MISMATCH");
                }
            }

            let mode = if warn_only { OnFailure::Warn } else { OnFailure::Reject };
            let verified =
                VerifiedLayerVfs::new(root, rebuilt, current_mf, mf.root_hash, mode);
            let (checked, failed) = verified
                .full_check()
                .map_err(|e| anyhow!("running full_check: {e}"))?;
            println!(
                "verified {} blocks, {} failed (root_hash match: {})",
                checked,
                failed,
                rebuilt_matches(&verified, &mf.root_hash)
            );
            if failed > 0 && !warn_only {
                bail!("verification failed for {} blocks", failed);
            }
        }
        VerityOp::Inspect { manifest } => {
            let mf_text = fs::read_to_string(&manifest)
                .with_context(|| format!("reading manifest {}", manifest.display()))?;
            let mf: LayerManifest = serde_json::from_str(&mf_text)?;
            println!(
                "root_hash:    {}",
                mf.root_hash
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            );
            println!(
                "manifest_hash:{}",
                mf.manifest_hash
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            );
            println!("block_size:   {}", mf.block_size);
            println!("file_count:   {}", mf.files.len());
            for f in &mf.files {
                println!(
                    "  {:>10} bytes  blocks {}..{}  {}",
                    f.size, f.block_range.0, f.block_range.1, f.path
                );
            }
        }
    }
    Ok(())
}

fn rebuilt_matches(verified: &VerifiedLayerVfs, expected: &[u8; 32]) -> bool {
    verified.root_hash() == expected
}
