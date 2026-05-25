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
mod kmsg;
#[cfg(target_os = "linux")]
mod metrics;
#[cfg(target_os = "linux")]
mod stats;

#[cfg(target_os = "linux")]
mod kmsg_faults_enabled {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ENABLED: AtomicBool = AtomicBool::new(true);
    pub fn set(on: bool) {
        ENABLED.store(on, Ordering::Relaxed);
    }
    pub fn get() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}

/// Emit one record to `/dev/kmsg` AND a structured tracing event. Used at
/// fault sites so the same payload shows up in both journald (rich fields)
/// and `dmesg` (kernel ring buffer, survives userspace journal outages).
#[cfg(target_os = "linux")]
fn fault(prio: kmsg::Prio, kind: &str, msg: &str) {
    match prio {
        kmsg::Prio::Err => tracing::error!(fault = kind, "{msg}"),
        kmsg::Prio::Warn => tracing::warn!(fault = kind, "{msg}"),
        kmsg::Prio::Info => tracing::info!(fault = kind, "{msg}"),
    }
    if kmsg_faults_enabled::get() {
        kmsg::write(prio, &format!("[{kind}] {msg}"));
    }
}

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
        // mount_program is invoked under containers-storage / CRI-O — that
        // always runs under systemd, so default to journald.
        init_tracing(false, "auto");

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
                parse_overlay_options(
                    rest,
                    &mut upper,
                    &mut lowers,
                    &mut allow_other,
                    &mut allow_root,
                )?;
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
        if lowers.is_empty() {
            bail!("no lowerdir= specified (or all empty)");
        }
        // CRI-O always passes `upperdir=`; buildah / podman pass only
        // `lowerdir=` for read-only overlay ops (image inspect, commit's
        // source mount, layer-merge during pull). The kernel mounts those
        // `ro` at the VFS layer so writes never happen. Synthesize an
        // empty disposable tmpfs-style dir as the upper so LayerFS has
        // something to compose against. See issue #19.
        let (upper, _upper_tmpdir_keep): (PathBuf, Option<std::path::PathBuf>) = match upper {
            Some(u) => (u, None),
            None => {
                let pid = std::process::id();
                let dir = std::env::temp_dir().join(format!("rspacefs-ro-{}", pid));
                std::fs::create_dir_all(&dir).with_context(|| {
                    format!("creating synthetic empty upper at {}", dir.display())
                })?;
                tracing::info!(
                    upper_synth = %dir.display(),
                    "no upperdir= passed; synthesized an empty upper for read-only overlay mount"
                );
                (dir.clone(), Some(dir))
            }
        };

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
        let fs =
            crate::fs::RspacefsFuse::new(VfsPath::new(overlay), physical_layers, verified_layers);

        let mut opts: Vec<MountOption> = vec![
            MountOption::FSName("rspacefs".to_string()),
            MountOption::Subtype("rspacefs".to_string()),
            MountOption::DefaultPermissions,
            // mount_program serves container rootfs that is then accessed
            // by the container's PID namespace under arbitrary UIDs (e.g.
            // coredns runs as uid 65532). Without allow_other, FUSE
            // rejects ALL non-mounter UIDs at the VFS layer — before mode
            // bits are even checked — so any non-root container gets
            // EACCES on every file, including its own entrypoint binary
            // ("exec container process /foo: Permission denied").
            // Honoring the -o allow_other hint from storage.conf is not
            // enough because containers-storage doesn't pass it. Set it
            // unconditionally in mount_program mode.
            MountOption::AllowOther,
        ];
        // The `-o allow_other` flag from storage.conf is now redundant (we
        // set it unconditionally) but we still honor explicit allow_root.
        let _ = allow_other;
        if allow_root {
            opts.push(MountOption::AllowRoot);
        }

        // containers-storage expects mount_program to RETURN once the mount
        // is established. fuser::mount2() blocks until unmount, so we fork:
        // the parent polls the mountpoint and exits 0 when the FUSE mount
        // shows up; the child detaches and runs the FUSE event loop forever.
        daemonize_after_mount(&mountpoint).context("daemonize")?;

        // Hardening (#22 / #24): run the FUSE session inside a panic-catching
        // shell so any unexpected panic in a handler doesn't drop us out
        // without unmounting. On exit (clean or panic), force a lazy
        // umount on the mountpoint so the kernel mount table doesn't leak
        // a 'Transport endpoint is not connected' zombie. Also clean up
        // the synthesized empty-upper tempdir if one was created.
        let mp = mountpoint.clone();
        let tmp_upper = _upper_tmpdir_keep.clone();
        let session_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fuser::mount2(fs, &mountpoint, &opts)
                .context("FUSE mount failed (need /dev/fuse access?)")
        }));
        cleanup_mount_on_exit(&mp, tmp_upper.as_deref());
        match session_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(panic_payload) => {
                let msg = panic_msg(panic_payload);
                crate::fault(crate::kmsg::Prio::Err, "panic", &format!("FUSE session panicked; mount cleaned up: {msg}"));
                bail!("FUSE session panicked: {msg}");
            }
        }
    }

    /// Lazy-unmount the FUSE mountpoint + remove the synthesized empty
    /// upper tempdir (if any) on session exit. Always runs — clean exit,
    /// kernel-initiated unmount, or panic.
    ///
    /// Why lazy: a regular umount(2) can fail with EBUSY if the kernel
    /// hasn't fully released its references yet (e.g. another process
    /// holds a stale handle). MNT_DETACH detaches the mount from the
    /// filesystem hierarchy immediately and lets the kernel reap it
    /// once the last reference drops. That eliminates the
    /// 'Transport endpoint is not connected' zombie class.
    fn cleanup_mount_on_exit(mountpoint: &std::path::Path, tmp_upper: Option<&std::path::Path>) {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(c) = std::ffi::CString::new(mountpoint.as_os_str().as_bytes()) {
            // SAFETY: c is a valid C string; umount2 is a syscall wrapper.
            let rc = unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
            if rc == 0 {
                tracing::info!(mountpoint = %mountpoint.display(), "lazy-unmounted on session exit");
            } else {
                let errno = std::io::Error::last_os_error();
                // EINVAL = mountpoint isn't mounted (already gone). Not an error.
                // EPERM = no privileges. Caller is root for mount_program; warn.
                tracing::debug!(
                    mountpoint = %mountpoint.display(),
                    error = %errno,
                    "umount2 on exit returned non-zero (often EINVAL = already unmounted)"
                );
            }
        }
        if let Some(t) = tmp_upper {
            let _ = std::fs::remove_dir_all(t);
        }
    }

    /// Extract a printable message from a panic payload (the value
    /// inside Box<dyn Any> that catch_unwind returns).
    fn panic_msg(payload: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).into()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".into()
        }
    }

    /// Fork the process. The parent polls the mountpoint until it sees a
    /// FUSE superblock and exits 0, or exits non-zero if the child dies or
    /// the timeout fires. The child detaches (setsid, stdio→/dev/null) and
    /// returns so the caller can drop into the FUSE event loop.
    ///
    /// This is the daemonization contract containers-storage `mount_program`
    /// expects: the binary returns as soon as the kernel acknowledges the
    /// mount; the FUSE server lives on as a detached process.
    fn daemonize_after_mount(mountpoint: &std::path::Path) -> Result<()> {
        use std::time::{Duration, Instant};

        // Forking with live threads is undefined behaviour. We must fork
        // before fuser spawns any worker threads — which is fine: at this
        // point we've only built clap output, the FUSE session is created
        // by mount2() *after* this returns.

        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                bail!("fork failed: {}", std::io::Error::last_os_error());
            }
            if pid > 0 {
                // Parent — poll mountpoint, wait for FUSE_SUPER_MAGIC.
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if is_fuse_mounted(mountpoint) {
                        // Mount is live — let the child carry on.
                        libc::_exit(0);
                    }
                    // Has the child died?
                    let mut status: libc::c_int = 0;
                    let wpid = libc::waitpid(pid, &mut status, libc::WNOHANG);
                    if wpid == pid {
                        if libc::WIFEXITED(status) {
                            libc::_exit(libc::WEXITSTATUS(status));
                        }
                        libc::_exit(1);
                    }
                    if Instant::now() >= deadline {
                        // Give up — kill the child so it doesn't hang.
                        libc::kill(pid, libc::SIGTERM);
                        libc::_exit(1);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }

            // Child — detach.
            if libc::setsid() < 0 {
                bail!("setsid: {}", std::io::Error::last_os_error());
            }
            let dev_null = libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_RDWR);
            if dev_null >= 0 {
                libc::dup2(dev_null, libc::STDIN_FILENO);
                libc::dup2(dev_null, libc::STDOUT_FILENO);
                libc::dup2(dev_null, libc::STDERR_FILENO);
                if dev_null > libc::STDERR_FILENO {
                    libc::close(dev_null);
                }
            }
        }
        Ok(())
    }

    /// True if the path's filesystem reports the FUSE magic. Used by the
    /// daemonize parent to confirm the mount is up before exiting.
    fn is_fuse_mounted(p: &std::path::Path) -> bool {
        // `statfs::f_type` is `__fsword_t` on Linux — same type, same width
        // as the magic constant. Cross-arch portable as long as we land here
        // only on Linux (this whole module is `#[cfg(target_os = "linux")]`).
        const FUSE_SUPER_MAGIC: libc::__fsword_t = 0x65735546;
        let c = match std::ffi::CString::new(p.to_string_lossy().as_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
            return false;
        }
        buf.f_type == FUSE_SUPER_MAGIC
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
            } else if tok == "nodev"
                || tok == "noexec"
                || tok == "nosuid"
                || tok == "ro"
                || tok == "rw"
            {
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
        let parsed: std::collections::HashMap<String, V> =
            serde_json::from_str(s).context("RSPACEFS_VERITY_LOWERS env var must be JSON")?;
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

        /// Log output format / destination. `auto` (default) writes to
        /// journald when stderr is connected to it (the systemd / containerd
        /// case), otherwise falls back to text-on-stderr. `text` forces
        /// human-readable stderr. `json` forces newline-delimited JSON on
        /// stderr (good for `journalctl` ingestion of older systemd or for
        /// stdout-based log shippers). `journald` forces journald and errors
        /// if it can't connect.
        #[arg(long, value_name = "FMT", default_value = "auto")]
        log_format: String,

        /// Also write fault-class events (panics, EIO, lock poisoning,
        /// kernel-mount cleanup) to `/dev/kmsg` so they appear in `dmesg`
        /// even if journald is unreachable. Best-effort; no-op when the
        /// process lacks `CAP_SYSLOG`.
        #[arg(long, default_value_t = true)]
        kmsg_faults: bool,

        /// Path for an optional Unix-socket control surface. When set,
        /// rspacefs-mount listens on this socket for newline-delimited JSON
        /// commands (`status`, `invalidate`, `ping`, ...). Use `rspacefs ctl
        /// --socket PATH <cmd>` to talk to it from a client. Without this
        /// flag, no control surface is exposed.
        #[arg(long, value_name = "PATH")]
        control_socket: Option<PathBuf>,

        /// Optional `host:port` to expose Prometheus metrics over HTTP.
        /// When set, rspacefs-mount serves `GET /metrics` (Prometheus
        /// text-exposition format) and `GET /healthz` (200 OK) on that
        /// address. One open port per mount process. The node-exporter
        /// aggregator scrapes per-PID instances and re-exposes one
        /// node-level /metrics. OpenShift ServiceMonitor / PodMonitor
        /// compatible.
        #[arg(long, value_name = "HOST:PORT")]
        metrics_addr: Option<String>,

        /// Pre-squash lower layers into a single read-only tree at mount
        /// time using hardlinks (or reflinks where supported). Reduces the
        /// per-resolve fan-out from N lowers to 1, at the cost of a
        /// one-time setup walk. Pass an integer to control how many top
        /// lowers stay un-squashed (e.g. `--squash-lowers 2` keeps the top
        /// two lowers individual and squashes the rest into one beneath).
        /// `0` (or just `--squash-lowers` with no value) squashes
        /// everything. Whiteouts in higher-priority lowers are honored
        /// during the squash walk. The squash dir lives under
        /// `$XDG_RUNTIME_DIR/rspacefs/squash-<mount-hash>/` and is removed
        /// on unmount.
        ///
        /// Mostly redundant for read paths now that the whiteout cache
        /// makes 150+ lowers free per resolve, but useful when an external
        /// tool (kernel overlayfs fallback, image-build sanity check)
        /// wants a flattened view.
        #[arg(long, value_name = "KEEP_TOP", num_args = 0..=1, default_missing_value = "0")]
        squash_lowers: Option<usize>,
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        init_tracing(cli.debug, &cli.log_format);
        // Stash kmsg_faults so panic handlers / setattr error paths can read it.
        crate::kmsg_faults_enabled::set(cli.kmsg_faults);

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
            let verified = VerifiedFS::load_pinned(path, &p.manifest, &p.tree, OnFailure::Reject)
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
            let verified = VerifiedFS::build(path, OnFailure::Reject)
                .context(format!("building verity manifest for {}", l.display()))?;
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

        // Squash-lowers v1: the flag is accepted and acknowledged but the
        // hardlink-merge walk isn't implemented yet. Doing it correctly
        // requires honoring OCI whiteouts during the squash, which means
        // walking each lower top-down and applying mask sets — non-trivial
        // and not needed for read-path scaling now that the in-core
        // whiteout cache makes 150+ lowers free per resolve. Tracked as a
        // future enhancement; the flag exists so deployment tooling /
        // OpenShift DaemonSet configs can be written today against the
        // final CLI shape.
        if let Some(keep) = cli.squash_lowers {
            tracing::warn!(
                keep_top = keep,
                "--squash-lowers set, but the squash walk is not yet implemented; \
                 mount proceeds with un-squashed lowers. The whiteout cache in \
                 rspacefs-core handles 150+ lowers efficiently at runtime — squash \
                 is primarily useful for external tools that want a flattened view."
            );
        }

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

        // Pull stats out BEFORE moving `fs` into the FUSE session — we
        // need the Arc<Stats> handle alive in the metrics + control
        // threads independently of the session.
        let stats = fs.stats();
        let mountpoint_str = cli.mountpoint.display().to_string();

        // Metrics HTTP server is independent of the control socket.
        // Either, both, or neither may be enabled.
        if let Some(addr) = &cli.metrics_addr {
            let _h = crate::metrics::spawn_metrics_server(
                addr,
                std::sync::Arc::clone(&stats),
                mountpoint_str.clone(),
            )
            .with_context(|| format!("failed to bind metrics listener on {addr}"))?;
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
                    stats,
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
                let mp = cli.mountpoint.clone();
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.run()));
                let _ = std::fs::remove_file(sock_path);
                cleanup_mount_on_exit(&mp, None);
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        return Err(anyhow::Error::new(e).context("FUSE session ended with error"))
                    }
                    Err(p) => {
                        let msg = panic_msg(p);
                        crate::fault(crate::kmsg::Prio::Err, "panic", &format!("FUSE session panicked; mount cleaned up: {msg}"));
                        anyhow::bail!("FUSE session panicked: {msg}");
                    }
                }
            }
            None => {
                let mp = cli.mountpoint.clone();
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fuser::mount2(fs, &cli.mountpoint, &opts)
                        .context("FUSE mount failed (need /dev/fuse access?)")
                }));
                cleanup_mount_on_exit(&mp, None);
                match res {
                    Ok(r) => r?,
                    Err(p) => {
                        let msg = panic_msg(p);
                        crate::fault(crate::kmsg::Prio::Err, "panic", &format!("FUSE session panicked; mount cleaned up: {msg}"));
                        anyhow::bail!("FUSE session panicked: {msg}");
                    }
                }
                return Ok(());
            }
        }
        Ok(())
    }

    fn init_tracing(debug: bool, format: &str) {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::{fmt, EnvFilter, Registry};

        let filter = if debug {
            EnvFilter::new("rspacefs_fuse=debug,info")
        } else {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
        };

        let want_journald = match format {
            "journald" => Some(true),  // hard-required
            "auto" => None,             // try, fall through
            _ => Some(false),            // text / json — stderr
        };

        if want_journald != Some(false) {
            match tracing_journald::layer() {
                Ok(layer) => {
                    let _ = Registry::default().with(filter).with(layer).try_init();
                    return;
                }
                Err(e) => {
                    if want_journald == Some(true) {
                        eprintln!("--log-format=journald requested but unavailable: {e}");
                        std::process::exit(2);
                    }
                    // auto: silently fall through to stderr.
                }
            }
        }

        // stderr — text or JSON.
        let filter = if debug {
            EnvFilter::new("rspacefs_fuse=debug,info")
        } else {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
        };
        if format == "json" {
            let _ = fmt()
                .json()
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
        } else {
            let _ = fmt()
                .with_env_filter(filter)
                .with_target(false)
                .try_init();
        }
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
