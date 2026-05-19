//! `rspacefs-mount` — mount an rspacefs overlay (and optional verified lowers) as a FUSE filesystem.
//!
//! Linux-only binary. On other host OSes this compiles to a stub that exits
//! with an explanatory error — the real FUSE bridge depends on `fuser`, which
//! pulls in libfuse / macFUSE and is only fully supported on Linux today.
//!
//! Usage (Linux):
//!   rspacefs-mount --upper ./upper --lower ./layer1 --lower ./layer2 /mnt/myroot
//!   rspacefs-mount --upper ./upper --lower-verified ./base --lower ./app /mnt/myroot
//!
//! Stop the mount with `fusermount -u /mnt/myroot` (or kill the process —
//! `--auto-unmount` is on by default).

#[cfg(target_os = "linux")]
mod fs;

#[cfg(target_os = "linux")]
mod linux_main {
    use std::path::PathBuf;

    use anyhow::{anyhow, bail, Context, Result};
    use clap::Parser;
    use fuser::MountOption;
    use rspacefs_core::LayerFS;
    use rspacefs_verity::{OnFailure, VerifiedFS};
    use vfs::{PhysicalFS, VfsPath};

    use crate::fs::RspacefsFuse;

    /// A `--lower-verified-pinned DIR=MANIFEST=TREE` triple.
    #[derive(Clone, Debug)]
    pub struct PinnedVerified {
        pub dir: PathBuf,
        pub manifest: PathBuf,
        pub tree: PathBuf,
    }

    fn parse_pinned_verified(s: &str) -> Result<PinnedVerified, String> {
        let parts: Vec<&str> = s.split('=').collect();
        if parts.len() != 3 {
            return Err(format!(
                "expected DIR=MANIFEST=TREE; got {} part(s) in {:?}",
                parts.len(),
                s
            ));
        }
        Ok(PinnedVerified {
            dir: PathBuf::from(parts[0]),
            manifest: PathBuf::from(parts[1]),
            tree: PathBuf::from(parts[2]),
        })
    }

    // Silence the unused-import warning when anyhow!() isn't used elsewhere.
    const _: fn() -> anyhow::Error = || anyhow!("unused");

    #[derive(Parser)]
    #[command(
        name = "rspacefs-mount",
        version,
        about = "Mount an rspacefs overlay (upper + lowers) at a FUSE mountpoint",
        long_about = None
    )]
    struct Cli {
        /// Writable upper layer (all writes through the mount land here).
        #[arg(long)]
        upper: PathBuf,

        /// Read-only lower layer (repeatable). Order is top-down: first
        /// `--lower` has the highest priority.
        #[arg(long, value_name = "DIR")]
        lower: Vec<PathBuf>,

        /// Read-only lower layer wrapped in verity verification. The Merkle
        /// tree is **rebuilt from current contents at mount time** — only
        /// useful for detecting tampering that happens AFTER the mount.
        /// For real tamper-evidence use `--lower-verified-pinned` with a
        /// manifest produced at image-build time.
        #[arg(long, value_name = "DIR")]
        lower_verified: Vec<PathBuf>,

        /// Read-only lower layer with a pre-built (pinned) verity manifest +
        /// tree from disk. Tampering of the underlying files between the
        /// manifest's build time and the mount is detected on first read.
        /// Format: `DIR=MANIFEST.json=TREE.bin`. Repeatable.
        #[arg(long, value_name = "DIR=MFS=TREE", value_parser = parse_pinned_verified)]
        lower_verified_pinned: Vec<PinnedVerified>,

        /// Mountpoint (must be an existing empty directory).
        mountpoint: PathBuf,

        /// Mount name to advertise (defaults to `rspacefs`).
        #[arg(long, default_value = "rspacefs")]
        name: String,

        /// Allow other users to access the mount (requires
        /// `user_allow_other` in /etc/fuse.conf).
        #[arg(long)]
        allow_other: bool,

        /// Allow root to access the mount even when started non-root.
        #[arg(long)]
        allow_root: bool,

        /// Auto-unmount when the mount process exits. Note: enabling this
        /// causes libfuse to also pass `allow_other`, which requires
        /// `user_allow_other` in `/etc/fuse.conf`. Off by default; on signal
        /// or daemon exit, run `fusermount3 -u <mountpoint>` manually.
        #[arg(long)]
        auto_unmount: bool,

        /// Show debug-level FUSE op logs.
        #[arg(long)]
        debug: bool,
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        init_tracing(cli.debug);

        if !cli.upper.is_dir() {
            bail!("upper layer is not a directory: {}", cli.upper.display());
        }
        if cli.lower.is_empty()
            && cli.lower_verified.is_empty()
            && cli.lower_verified_pinned.is_empty()
        {
            bail!("at least one --lower / --lower-verified / --lower-verified-pinned is required");
        }
        for l in cli.lower.iter().chain(cli.lower_verified.iter()) {
            if !l.is_dir() {
                bail!("lower layer is not a directory: {}", l.display());
            }
        }
        for p in &cli.lower_verified_pinned {
            if !p.dir.is_dir() {
                bail!("pinned lower is not a directory: {}", p.dir.display());
            }
            if !p.manifest.is_file() {
                bail!("manifest not found: {}", p.manifest.display());
            }
            if !p.tree.is_file() {
                bail!("tree not found: {}", p.tree.display());
            }
        }
        if !cli.mountpoint.is_dir() {
            bail!(
                "mountpoint is not a directory: {}",
                cli.mountpoint.display()
            );
        }

        let upper = VfsPath::new(PhysicalFS::new(cli.upper.clone()));

        // Layer priority: pinned-verified first (highest priority and most
        // trustworthy), then dynamic-verified (rebuilt at mount time), then
        // plain lowers. All physical-layer paths tracked for the FUSE
        // metadata side-channel.
        let mut lowers: Vec<VfsPath> = Vec::new();
        let mut physical_layers: Vec<std::path::PathBuf> = vec![cli.upper.clone()];
        for p in &cli.lower_verified_pinned {
            let path = VfsPath::new(PhysicalFS::new(p.dir.clone()));
            let verified = VerifiedFS::load_pinned(
                path,
                &p.manifest,
                &p.tree,
                OnFailure::Reject,
            )
            .context(format!(
                "loading pinned verity manifest for {} (manifest={}, tree={})",
                p.dir.display(),
                p.manifest.display(),
                p.tree.display(),
            ))?;
            lowers.push(verified.into());
            physical_layers.push(p.dir.clone());
        }
        for l in &cli.lower_verified {
            let path = VfsPath::new(PhysicalFS::new(l.clone()));
            let verified = VerifiedFS::build(path, OnFailure::Reject).context(
                format!("building verity manifest for {}", l.display()),
            )?;
            lowers.push(verified.into());
            physical_layers.push(l.clone());
        }
        for l in &cli.lower {
            lowers.push(VfsPath::new(PhysicalFS::new(l.clone())));
            physical_layers.push(l.clone());
        }

        tracing::info!(
            mountpoint = %cli.mountpoint.display(),
            upper = %cli.upper.display(),
            pinned_verified_layers = cli.lower_verified_pinned.len(),
            dynamic_verified_layers = cli.lower_verified.len(),
            plain_layers = cli.lower.len(),
            "starting rspacefs FUSE mount"
        );

        let overlay = LayerFS::new(upper, lowers);
        let fs = RspacefsFuse::new(VfsPath::new(overlay), physical_layers);

        let mut opts: Vec<MountOption> = vec![
            MountOption::FSName(cli.name.clone()),
            MountOption::Subtype("rspacefs".to_string()),
            MountOption::DefaultPermissions,
        ];
        if cli.allow_other {
            opts.push(MountOption::AllowOther);
        }
        if cli.allow_root {
            opts.push(MountOption::AllowRoot);
        }
        if cli.auto_unmount {
            opts.push(MountOption::AutoUnmount);
        }

        fuser::mount2(fs, &cli.mountpoint, &opts)
            .context("FUSE mount failed (need /dev/fuse access?)")?;
        Ok(())
    }

    fn init_tracing(debug: bool) {
        let filter = if debug {
            tracing_subscriber::EnvFilter::new("rspacefs_fuse=debug,info")
        } else {
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        };
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux_main::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "rspacefs-mount is a Linux-only binary (requires kernel FUSE + /dev/fuse). \
         Build and run on a Linux host."
    );
    std::process::exit(2);
}
