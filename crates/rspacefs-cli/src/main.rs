//! `rspacefs` — CLI for inspecting LayerFS mounts and managing verity layer manifests.
//!
//! Two subcommands: `overlay` (merge layers and read through them) and
//! `verity` (build / verify / inspect Merkle-tree layer manifests).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rspacefs_core::LayerFS;
use rspacefs_verity::{LayerManifest, MerkleTree, OnFailure, VerifiedLayerVfs, BLOCK_SIZE};
use vfs::{PhysicalFS, VfsPath};

#[derive(Parser)]
#[command(name = "rspacefs", version, about = "Userspace LayerFS + verity tools", long_about = None)]
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
    /// Talk to a running `rspacefs-mount` daemon via its control socket.
    Ctl {
        /// Path to the daemon's control socket (matches `--control-socket`
        /// passed to `rspacefs-mount`).
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
        #[command(subcommand)]
        op: CtlOp,
    },
    /// Offline PVC layer tools: scaffold, seed from a blob, capture to a
    /// blob. No daemon involved — see `rspacefs ctl` for live-mount ops
    /// and docs/pvc.md for the full lifecycle.
    Pvc {
        #[command(subcommand)]
        op: PvcOp,
    },
}

#[derive(Subcommand)]
enum PvcOp {
    /// Create an empty PVC upper scaffold directory.
    Init {
        #[arg(long)]
        upper: PathBuf,
    },
    /// Extract a pulled PVC blob (tar or tar+zstd) into a directory,
    /// pre-staging it as an upper or as a lower seed.
    Apply {
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        blob: PathBuf,
    },
    /// Capture an upper directory into a registry-pushable tar+zstd
    /// blob and print the report (path, sha256 digest, size) as JSON.
    Capture {
        #[arg(long)]
        upper: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
    },
}

#[derive(Subcommand)]
enum CtlOp {
    /// Round-trip a `ping` request — confirms the daemon is alive.
    Ping,
    /// Print the daemon's mount state as JSON.
    Status,
    /// Invalidate the kernel's dentry cache for all top-level entries.
    /// Forces the kernel to re-enter the daemon on next access; used after
    /// a manifest rotation, layer swap, or other live-state change.
    Invalidate,
    /// Snapshot of every counter and gauge as JSON.
    Stats,
    /// Counters in Prometheus text-exposition format. Pipe into a scraper
    /// or `curl | promtool check metrics -`.
    Metrics,
    /// Config + binary version + pid + fuse-passthrough flag.
    Info,
    /// Recent FUSE op ring (newest first). `--n` caps the count.
    Ops {
        #[arg(long, default_value_t = 32)]
        n: usize,
    },
    /// Internal-state dump for debugging: open handles, RSS, last-op
    /// timestamp, layer count.
    Debug,
    /// PVC mounts only: promote the live mount's upper to a new backing
    /// directory (tmpfs → disk). The new dir must be a pre-populated,
    /// content-identical copy of the current upper.
    PivotUpper {
        #[arg(long, value_name = "DIR")]
        new_upper: PathBuf,
    },
    /// PVC mounts only: snapshot the live mount's upper into a
    /// deterministic tar+zstd blob; prints the digest report.
    CaptureLayer {
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        #[arg(long)]
        zstd_level: Option<i32>,
        #[arg(long)]
        since: Option<String>,
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
    Inspect { manifest: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Overlay { op } => run_overlay(op),
        Cmd::Verity { op } => run_verity(op),
        Cmd::Ctl { socket, op } => run_ctl(socket, op),
        Cmd::Pvc { op } => run_pvc(op),
    }
}

// ── pvc: offline PVC layer tools ────────────────────────────────────────────

fn run_pvc(op: PvcOp) -> Result<()> {
    match op {
        PvcOp::Init { upper } => {
            fs::create_dir_all(&upper)
                .with_context(|| format!("creating PVC upper at {}", upper.display()))?;
            println!("initialized empty PVC upper at {}", upper.display());
        }
        PvcOp::Apply { upper, blob } => {
            let report = rspacefs_pvc::apply_blob(&blob, &upper)
                .map_err(|e| anyhow!("applying {}: {e}", blob.display()))?;
            println!(
                "applied {} entries ({} bytes) from {} into {}",
                report.entries,
                report.bytes_written,
                blob.display(),
                upper.display()
            );
        }
        PvcOp::Capture {
            upper,
            out,
            zstd_level,
        } => {
            if !upper.is_dir() {
                bail!("upper is not a directory: {}", upper.display());
            }
            let pvc = rspacefs_pvc::PvcMount::new(rspacefs_pvc::PvcOptions {
                access_mode: rspacefs_pvc::PvcAccessMode::ReadWriteOnce,
                lifecycle: rspacefs_pvc::PvcLifecycle::Persistent,
                name: upper
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "pvc".into()),
                upper: VfsPath::new(PhysicalFS::new(upper.clone())),
                lowers: vec![],
                owner: None,
                upper_physical: Some(upper),
            })
            .map_err(|e| anyhow!("constructing PVC over upper: {e}"))?;
            let report = rspacefs_pvc::capture_layer(
                &pvc,
                rspacefs_pvc::CaptureOptions {
                    out_path: out,
                    zstd_level,
                    since: None,
                },
            )
            .map_err(|e| anyhow!("capture failed: {e}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "out_path": report.out_path,
                    "digest": report.digest,
                    "bytes_compressed": report.bytes_compressed,
                    "entries": report.entries,
                }))?
            );
        }
    }
    Ok(())
}

// ── ctl: talk to a running rspacefs-mount daemon ────────────────────────────

fn run_ctl(socket: PathBuf, op: CtlOp) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    // `metrics` strips the JSON envelope and prints `.data` raw so the
    // output is valid Prometheus exposition text (curl-pipe-able into a
    // scraper). Every other op pretty-prints the response JSON.
    let raw_metrics = matches!(op, CtlOp::Metrics);
    let request: String = match op {
        CtlOp::Ping => r#"{"cmd":"ping"}"#.into(),
        CtlOp::Status => r#"{"cmd":"status"}"#.into(),
        CtlOp::Invalidate => r#"{"cmd":"invalidate"}"#.into(),
        CtlOp::Stats => r#"{"cmd":"stats"}"#.into(),
        CtlOp::Metrics => r#"{"cmd":"metrics-text"}"#.into(),
        CtlOp::Info => r#"{"cmd":"info"}"#.into(),
        CtlOp::Ops { n } => format!(r#"{{"cmd":"ops","n":{}}}"#, n),
        CtlOp::Debug => r#"{"cmd":"debug"}"#.into(),
        CtlOp::PivotUpper { new_upper } => {
            serde_json::json!({"cmd": "pivot-upper", "new_upper": new_upper}).to_string()
        }
        CtlOp::CaptureLayer {
            out,
            zstd_level,
            since,
        } => {
            let mut v = serde_json::json!({"cmd": "capture-layer", "out_path": out});
            if let Some(level) = zstd_level {
                v["zstd_level"] = level.into();
            }
            if let Some(s) = since {
                v["since"] = s.into();
            }
            v.to_string()
        }
    };

    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to control socket {}", socket.display()))?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    if raw_metrics {
        // Response envelope is {ok, data} where data is the Prometheus
        // text. Pull `data` out and print it verbatim.
        let v: serde_json::Value =
            serde_json::from_str(&line).context("daemon returned non-JSON response")?;
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            let msg = v.get("error").and_then(|x| x.as_str()).unwrap_or("unknown");
            bail!("metrics-text request failed: {}", msg);
        }
        let text = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
        print!("{}", text);
        return Ok(());
    }

    // Default: pretty-print JSON for readability.
    match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => print!("{}", line),
    }
    Ok(())
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
    Ok(VfsPath::new(LayerFS::new(upper_vfs, lower_vfs)))
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
        OverlayOp::Cat { upper, lower, path } => {
            let root = build_overlay(upper, lower)?;
            let mut file = root.join(&path)?.open_file()?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            std::io::stdout().write_all(&buf)?;
        }
        OverlayOp::Stat { upper, lower, path } => {
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
                eprintln!(
                    "wrote tree to {} ({} nodes)",
                    t.display(),
                    merkle.node_count()
                );
            }

            eprintln!(
                "root_hash: {}",
                mf.root_hash
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            );
            eprintln!(
                "files: {}, block_size: {} bytes",
                mf.files.len(),
                BLOCK_SIZE
            );
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
                    mf.root_hash
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
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

            let mode = if warn_only {
                OnFailure::Warn
            } else {
                OnFailure::Reject
            };
            let verified = VerifiedLayerVfs::new(root, rebuilt, current_mf, mf.root_hash, mode);
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
