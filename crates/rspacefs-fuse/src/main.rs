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

    use anyhow::{bail, Context, Result};
    use clap::Parser;
    use fuser::MountOption;
    use rspacefs_core::OverlayFS;
    use rspacefs_verity::{OnFailure, VerifiedFS};
    use vfs::{PhysicalFS, VfsPath};

    use crate::fs::RspacefsFuse;

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

        /// Read-only lower layer wrapped in verity verification. Tampered or
        /// modified files in this directory cause reads to fail.
        #[arg(long, value_name = "DIR")]
        lower_verified: Vec<PathBuf>,

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
        if cli.lower.is_empty() && cli.lower_verified.is_empty() {
            bail!("at least one --lower or --lower-verified is required");
        }
        for l in cli.lower.iter().chain(cli.lower_verified.iter()) {
            if !l.is_dir() {
                bail!("lower layer is not a directory: {}", l.display());
            }
        }
        if !cli.mountpoint.is_dir() {
            bail!(
                "mountpoint is not a directory: {}",
                cli.mountpoint.display()
            );
        }

        let upper = VfsPath::new(PhysicalFS::new(cli.upper.clone()));

        // Verified lowers go first (highest priority) — typical use is the
        // base image is verified and the app layer on top is not. Pass
        // multiple `--lower` flags if you need a different order.
        let mut lowers: Vec<VfsPath> = Vec::new();
        for l in &cli.lower_verified {
            let path = VfsPath::new(PhysicalFS::new(l.clone()));
            let verified = VerifiedFS::build(path, OnFailure::Reject).context(
                format!("building verity manifest for {}", l.display()),
            )?;
            lowers.push(verified.into());
        }
        for l in &cli.lower {
            lowers.push(VfsPath::new(PhysicalFS::new(l.clone())));
        }

        tracing::info!(
            mountpoint = %cli.mountpoint.display(),
            upper = %cli.upper.display(),
            verified_layers = cli.lower_verified.len(),
            plain_layers = cli.lower.len(),
            "starting rspacefs FUSE mount"
        );

        let overlay = OverlayFS::new(upper, lowers);
        let fs = RspacefsFuse::new(VfsPath::new(overlay));

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
