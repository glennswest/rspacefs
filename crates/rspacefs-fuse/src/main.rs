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
mod control;
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

    // ── containers-storage `mount_program` compatibility ────────────────────
    //
    // When containers-storage (used by podman, buildah, CRI-O) calls a
    // user-defined `mount_program`, the argv looks like:
    //
    //     mount_program [-o lowerdir=L1:L2,upperdir=U,workdir=W,opt1,opt2,...] /mnt/point
    //
    // This is the same contract `fuse-overlayfs` follows. Detecting it
    // distinguishes the storage.conf invocation from a direct user run.

    /// True if `argv` looks like a `mount_program` invocation. Heuristic: any
    /// of the recognised overlay options as a `-o ...` flag.
    pub fn looks_like_mount_program(argv: &[std::ffi::OsString]) -> bool {
        let mut iter = argv.iter().map(|s| s.to_string_lossy());
        let _ = iter.next();
        let mut prev_was_o = false;
        for arg in iter {
            if arg == "-o" || arg == "--options" {
                prev_was_o = true;
                continue;
            }
            if prev_was_o {
                return arg.contains("lowerdir=") || arg.contains("upperdir=");
            }
            // Inline form: `-olowerdir=...`
            if arg.starts_with("-o") && (arg.contains("lowerdir=") || arg.contains("upperdir=")) {
                return true;
            }
            prev_was_o = false;
        }
        false
    }

    /// Parse the overlay-style argv and dispatch into the mount.
    ///
    /// `RSPACEFS_VERITY_LOWERS` (optional env var) is a JSON map of
    /// `{ "/path/to/lower": { "manifest": "...", "tree": "..." } }`. Any
    /// lower listed here gets verity-pinned mode; lowers not listed are
    /// plain (passthrough-eligible).
    pub fn run_mount_program(argv: &[std::ffi::OsString]) -> Result<()> {
        init_tracing(false);

        let mut iter = argv.iter().peekable();
        let _ = iter.next(); // skip program name

        let mut upper: Option<PathBuf> = None;
        let mut lowers: Vec<PathBuf> = Vec::new();
        let mut mountpoint: Option<PathBuf> = None;
        let mut allow_other = false;
        let mut allow_root = false;

        while let Some(arg) = iter.next() {
            let s = arg.to_string_lossy().into_owned();
            if s == "-o" || s == "--options" {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow!("expected value after -o"))?;
                parse_overlay_options(
                    &val.to_string_lossy(),
                    &mut upper,
                    &mut lowers,
                    &mut allow_other,
                    &mut allow_root,
                )?;
            } else if let Some(rest) = s.strip_prefix("-o") {
                parse_overlay_options(rest, &mut upper, &mut lowers, &mut allow_other, &mut allow_root)?;
            } else if s == "-f" || s == "-d" || s == "-s" {
                // foreground / debug / single-threaded — accept and ignore;
                // we already run foreground & single-threaded enough for our
                // purposes.
            } else if s == "--help" || s == "-h" {
                print_mount_program_help();
                return Ok(());
            } else if s.starts_with('-') {
                tracing::warn!(option = %s, "ignoring unrecognised mount_program option");
            } else {
                if mountpoint.is_some() {
                    bail!("unexpected positional argument {:?}", s);
                }
                mountpoint = Some(PathBuf::from(&s));
            }
        }

        let mountpoint = mountpoint.ok_or_else(|| anyhow!("no mountpoint argument"))?;
        let upper = upper.ok_or_else(|| anyhow!("missing upperdir= in -o"))?;
        if lowers.is_empty() {
            bail!("no lowerdir= specified (or all empty)");
        }

        // Optional verity hints via env var.
        let verity_hints: std::collections::HashMap<PathBuf, (PathBuf, PathBuf)> =
            std::env::var("RSPACEFS_VERITY_LOWERS")
                .ok()
                .and_then(|s| parse_verity_hints_env(&s).ok())
                .unwrap_or_default();

        // Build vfs layer set + parallel physical_layers + verified_layers.
        let upper_vfs = VfsPath::new(PhysicalFS::new(upper.clone()));
        let mut lower_vfs: Vec<VfsPath> = Vec::new();
        let mut physical_layers: Vec<PathBuf> = vec![upper.clone()];
        let mut verified_layers: Vec<bool> = vec![false];

        for l in &lowers {
            if let Some((mfs, tree)) = verity_hints.get(l) {
                let path = VfsPath::new(PhysicalFS::new(l.clone()));
                let verified = VerifiedFS::load_pinned(path, mfs, tree, OnFailure::Reject)
                    .context(format!("loading pinned verity for {}", l.display()))?;
                lower_vfs.push(verified.into());
                physical_layers.push(l.clone());
                verified_layers.push(true);
            } else {
                lower_vfs.push(VfsPath::new(PhysicalFS::new(l.clone())));
                physical_layers.push(l.clone());
                verified_layers.push(false);
            }
        }

        tracing::info!(
            mountpoint = %mountpoint.display(),
            upper = %upper.display(),
            lowers = lowers.len(),
            verified_lowers = verity_hints.len(),
            "starting rspacefs FUSE mount (mount_program mode)"
        );

        let overlay = LayerFS::new(upper_vfs, lower_vfs);
        let fs = crate::fs::RspacefsFuse::new(VfsPath::new(overlay), physical_layers, verified_layers);

        let mut opts: Vec<MountOption> = vec![
            MountOption::FSName("rspacefs".to_string()),
            MountOption::Subtype("rspacefs".to_string()),
            MountOption::DefaultPermissions,
        ];
        if allow_other {
            opts.push(MountOption::AllowOther);
        }
        if allow_root {
            opts.push(MountOption::AllowRoot);
        }

        fuser::mount2(fs, &mountpoint, &opts)
            .context("FUSE mount failed (need /dev/fuse access?)")?;
        Ok(())
    }

    fn parse_overlay_options(
        opts: &str,
        upper: &mut Option<PathBuf>,
        lowers: &mut Vec<PathBuf>,
        allow_other: &mut bool,
        allow_root: &mut bool,
    ) -> Result<()> {
        for tok in opts.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(v) = tok.strip_prefix("lowerdir=") {
                for d in v.split(':') {
                    if !d.is_empty() {
                        lowers.push(PathBuf::from(d));
                    }
                }
            } else if let Some(v) = tok.strip_prefix("upperdir=") {
                *upper = Some(PathBuf::from(v));
            } else if tok.starts_with("workdir=") {
                // overlay's workdir is for staging temp content; we don't
                // need it — our copy-up is direct into upperdir.
            } else if tok == "allow_other" {
                *allow_other = true;
            } else if tok == "allow_root" {
                *allow_root = true;
            } else if tok == "volatile" {
                // No-op for now. (overlay 'volatile' = skip fsync; we don't
                // currently exploit it but accept silently.)
            } else if tok == "nodev" || tok == "noexec" || tok == "nosuid" || tok == "ro" || tok == "rw" {
                // VFS / kernel mount flags — set by the runtime, applied by
                // FUSE itself or `mount(8)`. Accept silently.
            } else if tok.starts_with("metacopy=")
                || tok.starts_with("redirect_dir=")
                || tok.starts_with("index=")
            {
                // overlayfs-specific tunables; not meaningful for us.
            } else {
                tracing::debug!(opt = %tok, "ignoring overlay option");
            }
        }
        Ok(())
    }

    fn parse_verity_hints_env(
        s: &str,
    ) -> Result<std::collections::HashMap<PathBuf, (PathBuf, PathBuf)>> {
        // JSON of { "/path": { "manifest": "...", "tree": "..." }, ... }
        #[derive(serde::Deserialize)]
        struct V {
            manifest: PathBuf,
            tree: PathBuf,
        }
        let parsed: std::collections::HashMap<String, V> = serde_json::from_str(s)
            .context("RSPACEFS_VERITY_LOWERS env var must be JSON")?;
        Ok(parsed
            .into_iter()
            .map(|(k, v)| (PathBuf::from(k), (v.manifest, v.tree)))
            .collect())
    }

    fn print_mount_program_help() {
        println!(
            "rspacefs-mount (containers-storage `mount_program` compatible mode)\n\
             \n\
             Usage:\n\
               rspacefs-mount [-f] [-o lowerdir=L1:L2,upperdir=U,workdir=W,opt,...] MOUNTPOINT\n\
             \n\
             Environment:\n\
               RSPACEFS_VERITY_LOWERS  JSON map of {{\"/lower/path\": {{\"manifest\": \"...\",\
             \"tree\": \"...\"}}}}; listed lowers are mounted in verity-pinned mode.\n\
             \n\
             Direct invocation (non-mount_program mode):\n\
               rspacefs-mount --upper DIR --lower DIR... [--lower-verified-pinned DIR=MFS=TREE] MOUNTPOINT\n\
            "
        );
    }

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

        /// Path for an optional Unix-socket control surface. When set,
        /// rspacefs-mount listens on this socket for newline-delimited JSON
        /// commands (`status`, `invalidate`, `ping`, ...). Use `rspacefs ctl
        /// --socket PATH <cmd>` to talk to it from a client. Without this
        /// flag, no control surface is exposed.
        #[arg(long, value_name = "PATH")]
        control_socket: Option<PathBuf>,
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
        // plain lowers. We also build a parallel `verified_layers` flag
        // vector — the FUSE adapter uses it to decide which reads can be
        // served via FUSE passthrough (non-verified) vs. must route through
        // the daemon for block-by-block verity (verified).
        let mut lowers: Vec<VfsPath> = Vec::new();
        let mut physical_layers: Vec<std::path::PathBuf> = vec![cli.upper.clone()];
        let mut verified_layers: Vec<bool> = vec![false]; // upper is writable, not verified
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
            verified_layers.push(true);
        }
        for l in &cli.lower_verified {
            let path = VfsPath::new(PhysicalFS::new(l.clone()));
            let verified = VerifiedFS::build(path, OnFailure::Reject).context(
                format!("building verity manifest for {}", l.display()),
            )?;
            lowers.push(verified.into());
            physical_layers.push(l.clone());
            verified_layers.push(true);
        }
        for l in &cli.lower {
            lowers.push(VfsPath::new(PhysicalFS::new(l.clone())));
            physical_layers.push(l.clone());
            verified_layers.push(false);
        }

        tracing::info!(
            mountpoint = %cli.mountpoint.display(),
            upper = %cli.upper.display(),
            pinned_verified_layers = cli.lower_verified_pinned.len(),
            dynamic_verified_layers = cli.lower_verified.len(),
            plain_layers = cli.lower.len(),
            "starting rspacefs FUSE mount"
        );

        let layered_root: VfsPath = LayerFS::new(upper, lowers.clone()).into();
        let fs = RspacefsFuse::new(
            layered_root.clone(),
            physical_layers.clone(),
            verified_layers.clone(),
        );

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

        // Branch: with control socket → Session::new + notifier() + se.run().
        // Without → mount2 (blocks until unmount; current behaviour).
        match &cli.control_socket {
            Some(sock_path) => {
                let control_state = std::sync::Arc::new(crate::control::ControlState {
                    mountpoint: cli.mountpoint.clone(),
                    upper: cli.upper.clone(),
                    lowers: physical_layers[1..].to_vec(),
                    verified_layers: verified_layers.clone(),
                    mount_time: std::time::SystemTime::now(),
                    root: layered_root,
                });
                // Session::new gives us the same blocking se.run() as
                // mount2() but lets us pull notifier() out first for the
                // control thread. (BackgroundSession::join unmounts on
                // call, which is the opposite of what we want.)
                let mut session = fuser::Session::new(fs, &cli.mountpoint, &opts)
                    .context("FUSE mount failed (need /dev/fuse access?)")?;
                let notifier = session.notifier();
                let _ctl = crate::control::spawn_control_thread(
                    sock_path.clone(),
                    control_state,
                    notifier,
                )
                .context("failed to start control socket")?;
                let res = session.run();
                let _ = std::fs::remove_file(sock_path);
                res.context("FUSE session ended with error")?;
            }
            None => {
                fuser::mount2(fs, &cli.mountpoint, &opts)
                    .context("FUSE mount failed (need /dev/fuse access?)")?;
            }
        }
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
    // Dispatch on argv shape:
    // 1. If invoked with overlay-style `[-o lowerdir=...,upperdir=...,workdir=...] /mountpoint`,
    //    parse those options — this is the containers-storage `mount_program`
    //    contract used by podman / buildah / CRI-O on OpenShift etc.
    // 2. Otherwise fall through to the native clap parser
    //    (--upper / --lower / --lower-verified-pinned / mountpoint).
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if linux_main::looks_like_mount_program(&raw) {
        return linux_main::run_mount_program(&raw);
    }
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
