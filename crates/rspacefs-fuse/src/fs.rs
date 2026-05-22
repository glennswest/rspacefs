//! FUSE adapter that exposes a `vfs::FileSystem` (typically a `LayerFS`)
//! as a kernel-mountable filesystem via `fuser`.
//!
//! Maps the kernel's inode-centric, byte-offset-addressed FUSE protocol onto
//! the path-based, stream-oriented `vfs::FileSystem` trait — but with one
//! escape hatch: for file *metadata* (mode bits, uid/gid, atime/mtime, nlink,
//! rdev) we sidestep the lossy `vfs::VfsMetadata` and stat the underlying
//! physical file directly. That preserves the executable bit, which is
//! critical for container rootfs use (`/usr/bin/sh` etc. need to be
//! `execve()`-able). Content I/O still goes through the overlay, so verity
//! verification on lower layers continues to apply.
//!
//! ## Limitations
//!
//! - File data is read in full on open and cached in the file-handle table.
//!   Fine for the typical container-rootfs read pattern (small config files,
//!   binaries with kernel page-cache backing); not ideal for huge files.
//!   A streaming `SeekAndRead` path is the obvious next optimisation.
//! - `setattr` accepts truncate-to-zero and falls back to "no-op success" for
//!   the rest. Container runtimes usually don't care; image builders sometimes
//!   do.
//! - Opaque-whiteout handling in the physical-resolution path matches simple
//!   per-entry whiteouts; ancestor opaque markers (`.wh..wh..opq` in an
//!   ancestor dir) fall back to overlay-reported metadata (synthesised mode).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    consts, BackingId, FileAttr, FileType, Filesystem, KernelConfig, PollHandle, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyPoll,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::{EBADF, EEXIST, EINVAL, EIO, ENOENT, ENOTEMPTY};
use vfs::{VfsFileType, VfsPath};

use crate::stats::{Op, Stats};

const ROOT_INO: u64 = 1;
const TTL: Duration = Duration::from_secs(1);
const FALLBACK_FILE_MODE: u16 = 0o644;
const FALLBACK_DIR_MODE: u16 = 0o755;
const BLOCK_SIZE: u32 = 4096;
const WHITEOUT_PREFIX: &str = ".wh.";

/// FUSE adapter wrapping a `vfs::FileSystem` (typically a `LayerFS`).
pub struct RspacefsFuse {
    /// The merged filesystem tree (overlay of upper + lowers). Used for all
    /// content I/O so verity-protected lowers stay verified on read.
    root: VfsPath,
    /// Physical layer directories in priority order: index 0 is the writable
    /// upper, indices 1.. are read-only lowers. Used as a side-channel to
    /// `stat()` the real backing file for accurate FileAttr (mode bits, uid,
    /// gid, times, nlink, rdev). The `vfs` crate doesn't expose any of those.
    layers: Vec<PathBuf>,
    /// Per-layer flag: `true` if this layer is verity-protected. Reads
    /// targeting a verity-protected layer must go through the daemon (so
    /// VerifiedFS hashes them); other reads can be served via FUSE
    /// passthrough — the kernel reads the backing file directly with zero
    /// daemon involvement.
    verified_layers: Vec<bool>,
    /// inode → relative path (UTF-8 / `/`-separated, with "" for root).
    inodes: HashMap<u64, String>,
    /// path → inode (reverse, for stable allocation across lookups).
    paths: HashMap<String, u64>,
    /// monotonic inode allocator.
    next_ino: u64,
    /// monotonic file-handle allocator.
    next_fh: AtomicU64,
    /// open file table: fh → cached content + dirty flag.
    open_files: HashMap<u64, OpenFile>,
    /// Per-inode BackingId cache. When the same file is opened by multiple
    /// containers / processes, we register ONE backing fd with the kernel
    /// (one `BACKING_OPEN` ioctl) and hand each opener a strong reference
    /// to the shared `Arc<BackingId>`. The kernel reclaims the backing
    /// when the last opener releases it.
    ///
    /// Weak references so that when the strong refs in the open_files
    /// table go away, the backing is freed. Look up + upgrade on each
    /// open; if upgrade succeeds, we reuse without a new ioctl.
    backing_cache: Mutex<HashMap<u64, Weak<BackingId>>>,
    /// uid/gid of the mounting process — used to populate fallback attrs
    /// when the underlying physical file can't be located (shouldn't happen
    /// for well-formed overlays, but `vfs` doesn't promise we can).
    fallback_uid: u32,
    fallback_gid: u32,
    /// Single timestamp stamped into fallback attrs.
    mount_time: SystemTime,
    /// Shared operational counters. Cloned into the control thread so
    /// `stats` / `metrics-text` requests don't need to lock the FS.
    pub(crate) stats: Arc<Stats>,
}

/// Per-open-file state. Two flavors:
///
/// - `Streaming`: read-only opens. Holds a `SeekAndRead` handle from the
///   overlay; `read(offset, size)` does seek+read, no whole-file buffer.
/// - `Buffered`: writable opens. Reads file content into memory at open
///   time, mutates the buffer on `write`, flushes on `release`.
///
/// Writable opens stay buffered because (a) partial writes need read-
/// modify-write inside a file, and (b) the vfs trait doesn't expose an
/// open-for-rw mode.
enum OpenFile {
    /// Read-only open of a non-verified file. The kernel reads the backing
    /// file directly via FUSE_PASSTHROUGH; our daemon never sees the
    /// `read()` ops. We hold a strong reference to the per-inode shared
    /// BackingId; when the last opener releases it, Drop fires
    /// (BACKING_CLOSE) and the kernel reclaims the backing registration.
    Passthrough { _backing: Arc<BackingId> },
    /// Read-only open with daemon-mediated I/O. Used when the file resolves
    /// to a verity-protected lower (so VerifiedFS hashes each block) or
    /// when passthrough setup fails on older kernels.
    Streaming {
        reader: Box<dyn vfs::SeekAndRead + Send>,
    },
    /// Writable open. Reads file into memory at open time, mutates the
    /// buffer on `write`, flushes on `release`/`fsync`.
    Buffered {
        path: String,
        data: Vec<u8>,
        dirty: bool,
        writable: bool,
    },
}

impl RspacefsFuse {
    /// Build a new FUSE adapter rooted at `root`.
    ///
    /// `layers` is the list of physical layer directories in priority order:
    /// index 0 is the writable upper, indices 1.. are read-only lowers. We
    /// keep these as a side-channel so we can `stat()` the real backing file
    /// for an accurate FileAttr (mode bits, uid/gid, times, nlink) — the
    /// `vfs` crate's metadata trait throws all of that away.
    pub fn new(root: VfsPath, layers: Vec<PathBuf>, verified_layers: Vec<bool>) -> Self {
        Self::new_with_stats(root, layers, verified_layers, Arc::new(Stats::new()))
    }

    /// Like `new`, but lets the caller hand in a pre-built `Stats` so the
    /// control thread can share it. The control surface needs an
    /// `Arc<Stats>` to read counters without locking the FS — easiest path
    /// is to build it once at startup and clone into both places.
    pub fn new_with_stats(
        root: VfsPath,
        layers: Vec<PathBuf>,
        verified_layers: Vec<bool>,
        stats: Arc<Stats>,
    ) -> Self {
        debug_assert_eq!(layers.len(), verified_layers.len());
        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(ROOT_INO, String::new());
        paths.insert(String::new(), ROOT_INO);
        Self {
            root,
            layers,
            verified_layers,
            inodes,
            paths,
            next_ino: ROOT_INO + 1,
            next_fh: AtomicU64::new(1),
            open_files: HashMap::new(),
            backing_cache: Mutex::new(HashMap::new()),
            fallback_uid: unsafe { libc::getuid() },
            fallback_gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
            stats,
        }
    }

    /// Public accessor for callers (the control thread) that need a
    /// reference to the shared `Stats` after the FS has been moved into
    /// the FUSE session.
    // dead_code allow goes away when control.rs grows Request::Stats /
    // Request::MetricsText / Request::Ops arms (task #8).
    #[allow(dead_code)]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    fn intern_path(&mut self, path: String) -> u64 {
        if let Some(&ino) = self.paths.get(&path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.paths.insert(path.clone(), ino);
        self.inodes.insert(ino, path);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<&str> {
        self.inodes.get(&ino).map(|s| s.as_str())
    }

    fn join(parent: &str, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return None;
        }
        Some(if parent.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", parent, name)
        })
    }

    /// Find the on-disk physical file backing a given overlay path. Walks
    /// layers in priority order; returns the resolved physical path and the
    /// layer index it came from. Honors entry-level whiteouts in the upper
    /// layer.
    fn physical_for_with_layer(&self, path: &str) -> Option<(PathBuf, usize)> {
        // Upper-layer whiteout: lower entries are masked.
        if let Some((parent, name)) = split_parent_name(path) {
            let mut wh = self.layers[0].clone();
            if !parent.is_empty() {
                wh.push(parent);
            }
            wh.push(format!("{}{}", WHITEOUT_PREFIX, name));
            if wh.symlink_metadata().is_ok() {
                return None;
            }
        }
        for (idx, layer) in self.layers.iter().enumerate() {
            let p = layer.join(path);
            if p.symlink_metadata().is_ok() {
                return Some((p, idx));
            }
        }
        None
    }

    /// Convenience wrapper for callers that only need the physical path.
    fn physical_for(&self, path: &str) -> Option<PathBuf> {
        self.physical_for_with_layer(path).map(|(p, _)| p)
    }

    /// Build a FileAttr from the physical file backing this overlay path. If
    /// the underlying file can't be stat'd, fall back to synthesised attrs
    /// derived from the overlay-reported `VfsFileType` and size.
    fn make_attr(&self, ino: u64, path: &str, ft: VfsFileType, size: u64) -> FileAttr {
        if let Some(phys) = self.physical_for(path) {
            if let Ok(m) = phys.symlink_metadata() {
                let kind = if m.file_type().is_dir() {
                    FileType::Directory
                } else if m.file_type().is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };
                return FileAttr {
                    ino,
                    size: m.len(),
                    blocks: m.len().div_ceil(BLOCK_SIZE as u64),
                    atime: ts_or_default(m.accessed().ok(), self.mount_time),
                    mtime: ts_or_default(m.modified().ok(), self.mount_time),
                    ctime: ctime_or_default(&m, self.mount_time),
                    crtime: ts_or_default(m.created().ok(), self.mount_time),
                    kind,
                    perm: (m.mode() as u16) & 0o7777,
                    nlink: m.nlink() as u32,
                    uid: m.uid(),
                    gid: m.gid(),
                    rdev: m.rdev() as u32,
                    blksize: BLOCK_SIZE,
                    flags: 0,
                };
            }
        }
        // Fallback when no physical file backs this path (shouldn't normally
        // happen — overlay said it exists but disk says otherwise).
        let (kind, perm, nlink) = match ft {
            VfsFileType::File => (FileType::RegularFile, FALLBACK_FILE_MODE, 1),
            VfsFileType::Directory => (FileType::Directory, FALLBACK_DIR_MODE, 2),
        };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(BLOCK_SIZE as u64),
            atime: self.mount_time,
            mtime: self.mount_time,
            ctime: self.mount_time,
            crtime: self.mount_time,
            kind,
            perm,
            nlink,
            uid: self.fallback_uid,
            gid: self.fallback_gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn attr_for_path(&self, ino: u64, path: &str) -> Option<FileAttr> {
        // Skip the vfs `metadata()` (which follows symlinks). The make_attr
        // implementation does its own symlink-preserving stat through
        // physical_for; we just need to confirm existence first.
        self.physical_for(path)?;
        Some(self.make_attr(ino, path, VfsFileType::File, 0))
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }

    /// Ensure `path` exists in the upper layer with its content + POSIX
    /// metadata + extended attributes copied up from whichever lower
    /// currently holds it. Returns the absolute upper-layer path.
    ///
    /// Used by xattr-write ops (setxattr/removexattr) and (in the future)
    /// other ops that need to mutate metadata without going through LayerFS's
    /// content-only `copy_up`. Uses `cp --reflink=auto` semantics: tries
    /// reflink first (instant on btrfs/xfs), falls back to a real copy.
    fn ensure_in_upper(&self, path: &str) -> Result<PathBuf, i32> {
        let upper_path = self.layers[0].join(path);
        if upper_path.symlink_metadata().is_ok() {
            return Ok(upper_path);
        }
        let source = self.physical_for(path).ok_or(ENOENT)?;
        let source_meta = source
            .symlink_metadata()
            .map_err(|e| e.raw_os_error().unwrap_or(EIO))?;

        // Ensure parent dirs in upper.
        if let Some(parent_dir) = upper_path.parent() {
            std::fs::create_dir_all(parent_dir).map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
        }

        // Copy the file: reflink first (instant on btrfs/xfs/apfs), fall back to a
        // byte copy. For symlinks, recreate the link. Both preserve content; the
        // helper below handles xattrs + mode after the fact.
        if source_meta.file_type().is_symlink() {
            let target =
                std::fs::read_link(&source).map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
            std::os::unix::fs::symlink(&target, &upper_path)
                .map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
        } else if source_meta.file_type().is_dir() {
            std::fs::create_dir(&upper_path).map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
        } else {
            copy_with_reflink(&source, &upper_path).map_err(|e| e.raw_os_error().unwrap_or(EIO))?;
        }

        // Preserve mode (don't touch symlinks — they don't have a mode of
        // their own that matters).
        if !source_meta.file_type().is_symlink() {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(source_meta.mode());
            let _ = std::fs::set_permissions(&upper_path, perms);
        }

        // Preserve xattrs.
        if let Ok(iter) = xattr::list(&source) {
            for name in iter {
                if let Ok(Some(val)) = xattr::get(&source, &name) {
                    let _ = xattr::set(&upper_path, &name, &val);
                }
            }
        }

        // Remove any whiteout marker for this path.
        let (parent, basename) = split_parent_name(path).unwrap_or(("", path));
        let mut wh = self.layers[0].clone();
        if !parent.is_empty() {
            wh.push(parent);
        }
        wh.push(format!("{}{}", WHITEOUT_PREFIX, basename));
        let _ = std::fs::remove_file(&wh);

        Ok(upper_path)
    }
}

// Internal helpers that work on owned/short-lived locals only — keeps
// borrow-checker noise out of each Filesystem method.

impl Filesystem for RspacefsFuse {
    // ── Init handshake ──────────────────────────────────────────────────────

    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        // Advertise passthrough capability to the kernel. If the running
        // kernel is < 6.9 (no passthrough support) this returns Err; we
        // ignore it and continue with daemon-mediated reads.
        if let Err(unsupported) = config.add_capabilities(consts::FUSE_PASSTHROUGH) {
            tracing::info!(
                "kernel does not support FUSE_PASSTHROUGH ({:?}); reads will route through the daemon",
                unsupported
            );
        } else {
            tracing::info!(
                "FUSE_PASSTHROUGH enabled — read-only opens of non-verified files bypass the daemon"
            );
        }

        // Perf tuning. The fuser default `max_write` is the FUSE legacy
        // 32 KB; modern kernels accept up to ~1 MB. Bump it. Same story
        // for readahead and the in-flight request window. Each of these
        // is a Result we silently ignore on error — they're hints, not
        // requirements.
        let _ = config.set_max_write(1 << 20); // 1 MB
        let _ = config.set_max_readahead(1 << 20); // 1 MB readahead
        let _ = config.set_max_background(64); // concurrent requests
        let _ = config.set_congestion_threshold(48); // backpressure floor

        Ok(())
    }

    // ── Lookup / metadata ────────────────────────────────────────────────────

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.stats.record(Op::Lookup, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };

        // Use the physical side-channel directly so symlinks are reported
        // AS symlinks (vfs follows them; FUSE consumers expect them raw).
        if self.physical_for(&path).is_none() {
            return reply.error(ENOENT);
        }

        let ino = self.intern_path(path.clone());
        // ft/size args ignored when physical_for succeeds (which we just verified).
        let attr = self.make_attr(ino, &path, VfsFileType::File, 0);
        reply.entry(&TTL, &attr, 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        self.stats.record(Op::Getattr, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        match self.attr_for_path(ino, &path) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(ENOENT),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        self.stats.record(Op::Setattr, ino, 0, 0);
        // We support the only setattr op containers actually rely on:
        // `truncate(path, 0)` to clobber a file. Everything else returns the
        // current attrs unchanged (kernel still sees a successful chmod, etc.)
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        if matches!(size, Some(0)) {
            let p = match self.root.join(&path) {
                Ok(v) => v,
                Err(_) => return reply.error(EIO),
            };
            if p.create_file().is_err() {
                return reply.error(EIO);
            }
        }
        match self.attr_for_path(ino, &path) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(ENOENT),
        }
    }

    // ── Directories ──────────────────────────────────────────────────────────

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        self.stats.record(Op::Readdir, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let dir = match self.root.join(&path) {
            Ok(d) => d,
            Err(_) => return reply.error(EIO),
        };
        let entries: Vec<String> = match dir.read_dir() {
            Ok(it) => it.map(|e| e.filename()).collect(),
            Err(_) => return reply.error(EIO),
        };

        // Synthesize ".", ".." plus the directory contents.
        let mut all: Vec<(u64, FileType, String)> = Vec::with_capacity(entries.len() + 2);
        all.push((ino, FileType::Directory, ".".to_string()));
        // ".." inode: cheap approximation — root's parent is root.
        let parent_ino = if ino == ROOT_INO {
            ROOT_INO
        } else {
            let parent_path = match path.rfind('/') {
                Some(i) => &path[..i],
                None => "",
            };
            *self.paths.get(parent_path).unwrap_or(&ROOT_INO)
        };
        all.push((parent_ino, FileType::Directory, "..".to_string()));

        for name in entries {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", path, name)
            };
            // Determine the entry kind via physical symlink_metadata so symlinks
            // show as symlinks (not as their targets).
            let kind = match self
                .physical_for(&child_path)
                .and_then(|p| p.symlink_metadata().ok())
            {
                Some(m) if m.file_type().is_dir() => FileType::Directory,
                Some(m) if m.file_type().is_symlink() => FileType::Symlink,
                Some(_) => FileType::RegularFile,
                None => FileType::RegularFile,
            };
            let child_ino = self.intern_path(child_path);
            all.push((child_ino, kind, name));
        }

        for (i, (e_ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            // `add` returns true if the buffer is full.
            if reply.add(e_ino, (i + 1) as i64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        self.stats.record(Op::Mkdir, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        if p.exists().unwrap_or(false) {
            return reply.error(EEXIST);
        }
        if p.create_dir().is_err() {
            return reply.error(EIO);
        }
        let ino = self.intern_path(path.clone());
        let attr = self.make_attr(ino, &path, VfsFileType::Directory, 0);
        reply.entry(&TTL, &attr, 0);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.stats.record(Op::Rmdir, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        // Empty-dir check: ENOTEMPTY if the directory has any entries.
        if let Ok(mut it) = p.read_dir() {
            if it.next().is_some() {
                return reply.error(ENOTEMPTY);
            }
        }
        if p.remove_dir().is_err() {
            return reply.error(EIO);
        }
        // Drop the inode binding — kernel forgets soon afterwards.
        if let Some(ino) = self.paths.remove(&path) {
            self.inodes.remove(&ino);
        }
        reply.ok();
    }

    // ── Files ────────────────────────────────────────────────────────────────

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        self.stats.record(Op::Open, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };

        let accmode = flags & libc::O_ACCMODE;
        let writable = accmode == libc::O_WRONLY || accmode == libc::O_RDWR;
        let fh = self.alloc_fh();

        if writable {
            // Writable: buffer-then-flush. Open via the vfs root so write-
            // back goes through LayerFS (which does its own copy-up).
            let p = match self.root.join(&path) {
                Ok(v) => v,
                Err(_) => return reply.error(EIO),
            };
            if !p.exists().unwrap_or(false) {
                return reply.error(ENOENT);
            }
            let mut data = Vec::new();
            if (flags & libc::O_TRUNC) == 0 {
                if let Ok(mut f) = p.open_file() {
                    if f.read_to_end(&mut data).is_err() {
                        return reply.error(EIO);
                    }
                }
            }
            self.stats.buffered_opens.fetch_add(1, Ordering::Relaxed);
            self.stats.open_handles.fetch_add(1, Ordering::Relaxed);
            self.open_files.insert(
                fh,
                OpenFile::Buffered {
                    path,
                    data,
                    dirty: false,
                    writable: true,
                },
            );
            return reply.opened(fh, 0);
        }

        // Read-only: try passthrough first. Passthrough hands the kernel a
        // direct fd to the backing file so subsequent reads NEVER hit our
        // daemon. Only valid when the resolved physical file lives in a
        // non-verified layer; verity-protected layers must stay on the
        // daemon path so block hashes are checked.
        if let Some((phys, layer_idx)) = self.physical_for_with_layer(&path) {
            let verified = *self.verified_layers.get(layer_idx).unwrap_or(&false);
            if !verified {
                // BackingId cache: if this inode already has a live
                // BackingId (because another open hasn't released yet),
                // reuse it. One BACKING_OPEN ioctl per file, regardless
                // of how many concurrent opens exist.
                let mut cache = match self.backing_cache.lock() {
                    Ok(c) => c,
                    Err(_) => return reply.error(EIO),
                };
                let existing = cache.get(&ino).and_then(|w| w.upgrade());
                if let Some(backing) = existing {
                    drop(cache);
                    self.stats
                        .backing_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    self.stats.passthrough_opens.fetch_add(1, Ordering::Relaxed);
                    self.stats.open_handles.fetch_add(1, Ordering::Relaxed);
                    reply.opened_passthrough(fh, 0, &backing);
                    self.open_files
                        .insert(fh, OpenFile::Passthrough { _backing: backing });
                    return;
                }
                // Cache miss — try to open + register a fresh backing.
                self.stats
                    .backing_cache_misses
                    .fetch_add(1, Ordering::Relaxed);
                if let Ok(backing_file) = std::fs::File::open(&phys) {
                    match reply.open_backing(&backing_file) {
                        Ok(backing) => {
                            let backing = Arc::new(backing);
                            cache.insert(ino, Arc::downgrade(&backing));
                            drop(cache);
                            self.stats.passthrough_opens.fetch_add(1, Ordering::Relaxed);
                            self.stats.open_handles.fetch_add(1, Ordering::Relaxed);
                            reply.opened_passthrough(fh, 0, &backing);
                            self.open_files
                                .insert(fh, OpenFile::Passthrough { _backing: backing });
                            return;
                        }
                        Err(e) => {
                            tracing::debug!(
                                "passthrough open failed for {} ({}); falling back to daemon read",
                                path,
                                e
                            );
                        }
                    }
                }
                drop(cache);
            }
        }

        // Fallback / verity path: stream from a SeekAndRead via the overlay
        // (VerifiedFS hashes each block when applicable).
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        if !p.exists().unwrap_or(false) {
            return reply.error(ENOENT);
        }
        match p.open_file() {
            Ok(reader) => {
                self.stats.streaming_opens.fetch_add(1, Ordering::Relaxed);
                self.stats.open_handles.fetch_add(1, Ordering::Relaxed);
                self.open_files.insert(fh, OpenFile::Streaming { reader });
                reply.opened(fh, 0);
            }
            Err(_) => reply.error(EIO),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(file) = self.open_files.get_mut(&fh) else {
            self.stats.record(Op::Read, ino, 0, EBADF);
            return reply.error(EBADF);
        };
        match file {
            // Kernel should be serving these directly via passthrough; if we
            // see a read on a passthrough handle something has slipped.
            OpenFile::Passthrough { .. } => {
                self.stats.record(Op::Read, ino, 0, EIO);
                reply.error(EIO)
            }
            OpenFile::Streaming { reader } => {
                if reader.seek(SeekFrom::Start(offset as u64)).is_err() {
                    self.stats.record(Op::Read, ino, 0, EIO);
                    return reply.error(EIO);
                }
                let mut buf = vec![0u8; size as usize];
                match reader.read(&mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        self.stats.record(Op::Read, ino, n as u64, 0);
                        reply.data(&buf);
                    }
                    Err(_) => {
                        self.stats.record(Op::Read, ino, 0, EIO);
                        reply.error(EIO)
                    }
                }
            }
            OpenFile::Buffered { data, .. } => {
                let start = offset as usize;
                if start >= data.len() {
                    self.stats.record(Op::Read, ino, 0, 0);
                    return reply.data(&[]);
                }
                let end = (start + size as usize).min(data.len());
                self.stats.record(Op::Read, ino, (end - start) as u64, 0);
                reply.data(&data[start..end]);
            }
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(file) = self.open_files.get_mut(&fh) else {
            self.stats.record(Op::Write, ino, 0, EBADF);
            return reply.error(EBADF);
        };
        match file {
            OpenFile::Passthrough { .. } | OpenFile::Streaming { .. } => {
                self.stats.record(Op::Write, ino, 0, libc::EACCES);
                reply.error(libc::EACCES)
            }
            OpenFile::Buffered {
                data: buf,
                dirty,
                writable,
                ..
            } => {
                if !*writable {
                    self.stats.record(Op::Write, ino, 0, libc::EACCES);
                    return reply.error(libc::EACCES);
                }
                let off = offset as usize;
                let end = off + data.len();
                if end > buf.len() {
                    buf.resize(end, 0);
                }
                buf[off..end].copy_from_slice(data);
                *dirty = true;
                self.stats.record(Op::Write, ino, data.len() as u64, 0);
                reply.written(data.len() as u32);
            }
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.stats.record(Op::Release, ino, 0, 0);
        self.stats.open_handles.fetch_sub(1, Ordering::Relaxed);
        let Some(file) = self.open_files.remove(&fh) else {
            return reply.error(EBADF);
        };
        match file {
            // Drop on BackingId fires BACKING_CLOSE in the kernel.
            OpenFile::Passthrough { .. } => reply.ok(),
            OpenFile::Streaming { .. } => reply.ok(),
            OpenFile::Buffered {
                path, data, dirty, ..
            } => {
                if dirty {
                    let p = match self.root.join(&path) {
                        Ok(v) => v,
                        Err(_) => return reply.error(EIO),
                    };
                    let mut w = match p.create_file() {
                        Ok(w) => w,
                        Err(_) => return reply.error(EIO),
                    };
                    if w.write_all(&data).is_err() || w.flush().is_err() {
                        return reply.error(EIO);
                    }
                }
                reply.ok();
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.stats.record(Op::Create, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        // Open-or-create semantics: if file exists and O_EXCL not set, treat as
        // open. If O_EXCL is set, fail with EEXIST when the entry already exists.
        let exists = p.exists().unwrap_or(false);
        if exists && (flags & libc::O_EXCL) != 0 {
            return reply.error(EEXIST);
        }
        if !exists {
            if p.create_file().is_err() {
                return reply.error(EIO);
            }
        } else if (flags & libc::O_TRUNC) != 0 {
            // O_TRUNC on existing file — clobber.
            if p.create_file().is_err() {
                return reply.error(EIO);
            }
        }

        let ino = self.intern_path(path.clone());
        let attr = self.make_attr(ino, &path, VfsFileType::File, 0);
        let fh = self.alloc_fh();
        self.stats.buffered_opens.fetch_add(1, Ordering::Relaxed);
        self.stats.open_handles.fetch_add(1, Ordering::Relaxed);
        self.open_files.insert(
            fh,
            OpenFile::Buffered {
                path,
                data: Vec::new(),
                dirty: false,
                writable: true,
            },
        );
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.stats.record(Op::Unlink, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        if !p.exists().unwrap_or(false) {
            return reply.error(ENOENT);
        }
        if p.remove_file().is_err() {
            return reply.error(EIO);
        }
        if let Some(ino) = self.paths.remove(&path) {
            self.inodes.remove(&ino);
        }
        reply.ok();
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        self.stats.record(Op::Rename, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let new_parent_path = match self.path_of(newparent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(src) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };
        let Some(dst) = Self::join(&new_parent_path, newname) else {
            return reply.error(EINVAL);
        };

        let src_p = match self.root.join(&src) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };
        let is_dir = matches!(
            src_p.metadata().map(|m| m.file_type),
            Ok(VfsFileType::Directory)
        );

        let res = if is_dir {
            src_p.move_dir(&match self.root.join(&dst) {
                Ok(v) => v,
                Err(_) => return reply.error(EIO),
            })
        } else {
            src_p.move_file(&match self.root.join(&dst) {
                Ok(v) => v,
                Err(_) => return reply.error(EIO),
            })
        };

        match res {
            Ok(_) => {
                // Update inode bindings.
                if let Some(ino) = self.paths.remove(&src) {
                    self.inodes.insert(ino, dst.clone());
                    self.paths.insert(dst, ino);
                }
                reply.ok();
            }
            Err(_) => reply.error(EIO),
        }
    }

    // ── Symlinks ────────────────────────────────────────────────────────────

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        self.stats.record(Op::Readlink, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let phys = match self.physical_for(&path) {
            Some(p) => p,
            None => return reply.error(ENOENT),
        };
        match std::fs::read_link(&phys) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        self.stats.record(Op::Symlink, parent, 0, 0);
        let parent_path = match self.path_of(parent) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let Some(path) = Self::join(&parent_path, name) else {
            return reply.error(EINVAL);
        };

        // Symlinks always go into the upper layer (writable, index 0).
        // First clear any whiteout marker on this path so the new symlink
        // is visible to the merge view.
        let mut wh = self.layers[0].clone();
        if !parent_path.is_empty() {
            wh.push(&parent_path);
        }
        wh.push(format!("{}{}", WHITEOUT_PREFIX, name.to_string_lossy()));
        let _ = std::fs::remove_file(&wh);

        // Ensure parent dirs exist in upper.
        let upper_path = self.layers[0].join(&path);
        if let Some(parent_dir) = upper_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent_dir) {
                return reply.error(e.raw_os_error().unwrap_or(EIO));
            }
        }

        match std::os::unix::fs::symlink(link, &upper_path) {
            Ok(_) => {
                let ino = self.intern_path(path.clone());
                let attr = self.make_attr(ino, &path, VfsFileType::File, 0);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    // ── Extended attributes ─────────────────────────────────────────────────

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        self.stats.record(Op::Getxattr, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let phys = match self.physical_for(&path) {
            Some(p) => p,
            None => return reply.error(ENOENT),
        };
        match xattr::get(&phys, name) {
            Ok(Some(val)) => {
                if size == 0 {
                    reply.size(val.len() as u32);
                } else if (size as usize) < val.len() {
                    reply.error(libc::ERANGE);
                } else {
                    reply.data(&val);
                }
            }
            Ok(None) => reply.error(libc::ENODATA),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        self.stats.record(Op::Listxattr, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let phys = match self.physical_for(&path) {
            Some(p) => p,
            None => return reply.error(ENOENT),
        };
        match xattr::list(&phys) {
            Ok(iter) => {
                // FUSE expects a NUL-separated, NUL-terminated list of names.
                let mut buf = Vec::new();
                for name in iter {
                    buf.extend_from_slice(name.as_bytes());
                    buf.push(0);
                }
                if size == 0 {
                    reply.size(buf.len() as u32);
                } else if (size as usize) < buf.len() {
                    reply.error(libc::ERANGE);
                } else {
                    reply.data(&buf);
                }
            }
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        self.stats.record(Op::Setxattr, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        // xattr writes need a writable target; ensure the file is in upper.
        let upper = match self.ensure_in_upper(&path) {
            Ok(p) => p,
            Err(errno) => return reply.error(errno),
        };
        match xattr::set(&upper, name, value) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        self.stats.record(Op::Removexattr, ino, 0, 0);
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let upper = match self.ensure_in_upper(&path) {
            Ok(p) => p,
            Err(errno) => return reply.error(errno),
        };
        match xattr::remove(&upper, name) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.raw_os_error().unwrap_or(EIO)),
        }
    }

    // ── poll(2) ─────────────────────────────────────────────────────────────

    fn poll(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _ph: PollHandle,
        events: u32,
        _flags: u32,
        reply: ReplyPoll,
    ) {
        self.stats.record(Op::Poll, ino, 0, 0);
        // Regular files and directories on a plain filesystem are *always*
        // poll-ready in the POSIX sense — read() / write() don't block on
        // them like they do on sockets / pipes / FIFOs. Echo back exactly
        // the events the caller asked about so select(2) / poll(2) /
        // epoll(2) all see "ready" and move on.
        //
        // This matches what tmpfs / ext4 / overlay would return for the
        // same fds; container runtimes (notably crun / runc when probing
        // /proc fds in the merged tree) expect this behaviour.
        reply.poll(events);
    }

    // ── Durable writes ──────────────────────────────────────────────────────

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        self.stats.record(Op::Fsync, ino, 0, 0);
        // For Buffered (writable) handles: flush in-memory dirty data to
        // the upper file now. Streaming (read-only) handles have nothing
        // to sync — return OK.
        let Some(file) = self.open_files.get_mut(&fh) else {
            return reply.error(EBADF);
        };
        match file {
            OpenFile::Passthrough { .. } => reply.ok(),
            OpenFile::Streaming { .. } => reply.ok(),
            OpenFile::Buffered {
                path, data, dirty, ..
            } => {
                if !*dirty {
                    return reply.ok();
                }
                let p = match self.root.join(path) {
                    Ok(v) => v,
                    Err(_) => return reply.error(EIO),
                };
                let mut w = match p.create_file() {
                    Ok(w) => w,
                    Err(_) => return reply.error(EIO),
                };
                if w.write_all(data).is_err() || w.flush().is_err() {
                    return reply.error(EIO);
                }
                *dirty = false;
                reply.ok();
            }
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        self.stats.record(Op::Flush, ino, 0, 0);
        // Per POSIX: close() may flush; the kernel calls flush per close.
        // We do the actual write-back in release() to coalesce, so flush
        // is a no-op for our buffered model. Returning OK lets close()
        // complete without spurious EIO.
        reply.ok();
    }

    // ── Statfs ──────────────────────────────────────────────────────────────

    fn statfs(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyStatfs) {
        self.stats.record(Op::Statfs, ino, 0, 0);
        // Synthesised — we don't know the underlying disk's real numbers
        // without poking the upper layer's backing filesystem. Report large
        // dummy values so callers don't think they're out of space.
        reply.statfs(
            1 << 30, // total blocks
            1 << 30, // free blocks
            1 << 30, // avail blocks
            1 << 20, // total files
            1 << 20, // free files
            BLOCK_SIZE,
            255, // max name length
            BLOCK_SIZE,
        );
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Split a `/`-separated path into `(parent, name)`. Returns None for empty
/// input (the root has no parent/name pair).
fn split_parent_name(path: &str) -> Option<(&str, &str)> {
    if path.is_empty() {
        return None;
    }
    match path.rfind('/') {
        Some(i) => Some((&path[..i], &path[i + 1..])),
        None => Some(("", path)),
    }
}

fn ts_or_default(t: Option<SystemTime>, fallback: SystemTime) -> SystemTime {
    t.unwrap_or(fallback)
}

/// `MetadataExt::ctime()` returns the change-time as `(sec, nsec)` ints —
/// build a SystemTime from those, or fall back if either is out of range.
fn ctime_or_default(m: &std::fs::Metadata, fallback: SystemTime) -> SystemTime {
    let sec = m.ctime();
    let nsec = m.ctime_nsec();
    if sec < 0 || nsec < 0 {
        return fallback;
    }
    UNIX_EPOCH
        .checked_add(Duration::new(sec as u64, nsec as u32))
        .unwrap_or(fallback)
}

/// Copy a regular file from `src` to `dst`, preferring a reflink
/// (copy-on-write) on filesystems that support it. Falls back to a
/// byte-level `std::fs::copy` if reflink isn't supported (ext4, etc.).
///
/// Linux's `ioctl_ficlone` is the kernel API for reflinks (btrfs, xfs ≥
/// 5.x with reflink=1, bcachefs, apfs via macOS reflink). Failure returns
/// `EINVAL` or `ENOTTY` on filesystems that don't support it; we silently
/// fall back.
fn copy_with_reflink(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // FICLONE = _IOW(0x94, 9, int)
    const FICLONE: libc::c_ulong = 0x40049409;

    let src_f = std::fs::File::open(src)?;
    let dst_f = std::fs::File::create(dst)?;
    let ret = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) };
    if ret == 0 {
        return Ok(());
    }
    // Reflink unsupported — drop the empty dst we just created and do
    // a regular copy.
    drop(src_f);
    drop(dst_f);
    let _ = std::fs::remove_file(dst);
    std::fs::copy(src, dst)?;
    Ok(())
}

// Silence unused-import warnings on uncommon configurations.
const _: fn(&Path) = |_| {};
