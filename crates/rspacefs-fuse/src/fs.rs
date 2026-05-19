//! FUSE adapter that exposes a `vfs::FileSystem` (typically an `OverlayFS`)
//! as a kernel-mountable filesystem via `fuser`.
//!
//! Maps the kernel's inode-centric, byte-offset-addressed FUSE protocol onto
//! the path-based, stream-oriented `vfs::FileSystem` trait.
//!
//! ## Limitations
//!
//! - `vfs::VfsMetadata` only carries `file_type` and `len`. Mode bits, uid/gid,
//!   atime/mtime/ctime are **synthesised**: files get `0o644`, directories
//!   `0o755`, owned by the mounting process, with all timestamps set to the
//!   mount-process start time. Full POSIX preservation needs an extended
//!   metadata trait on top of `vfs`.
//! - File data is read in full on open and cached in the file-handle table.
//!   Fine for the typical container-rootfs read pattern (small config files,
//!   binaries with kernel page-cache backing); not ideal for huge files.
//!   A streaming `SeekAndRead` path is the obvious next optimisation.
//! - `setattr` accepts truncate-to-zero and falls back to "no-op success" for
//!   the rest. Container runtimes usually don't care; image builders sometimes
//!   do.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use libc::{EBADF, EEXIST, EINVAL, EIO, ENOENT, ENOTEMPTY};
use vfs::{VfsFileType, VfsPath};

const ROOT_INO: u64 = 1;
const TTL: Duration = Duration::from_secs(1);
const FILE_MODE: u16 = 0o644;
const DIR_MODE: u16 = 0o755;
const BLOCK_SIZE: u32 = 4096;

/// FUSE adapter wrapping a `vfs::FileSystem` (typically an `OverlayFS`).
pub struct RspacefsFuse {
    /// The merged filesystem tree (overlay of upper + lowers).
    root: VfsPath,
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
    /// uid/gid of the mounting process — used to populate synthesised attrs.
    uid: u32,
    gid: u32,
    /// Single timestamp stamped into all synthesised attrs (mount start).
    mount_time: SystemTime,
}

struct OpenFile {
    path: String,
    /// Cached content (read in on open; written back on release if dirty).
    data: Vec<u8>,
    dirty: bool,
    writable: bool,
}

impl RspacefsFuse {
    /// Build a new FUSE adapter rooted at `root`.
    pub fn new(root: VfsPath) -> Self {
        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        inodes.insert(ROOT_INO, String::new());
        paths.insert(String::new(), ROOT_INO);
        Self {
            root,
            inodes,
            paths,
            next_ino: ROOT_INO + 1,
            next_fh: AtomicU64::new(1),
            open_files: HashMap::new(),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            mount_time: SystemTime::now(),
        }
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

    fn make_attr(&self, ino: u64, ft: VfsFileType, size: u64) -> FileAttr {
        let (kind, perm, nlink) = match ft {
            VfsFileType::File => (FileType::RegularFile, FILE_MODE, 1),
            VfsFileType::Directory => (FileType::Directory, DIR_MODE, 2),
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
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn attr_for_path(&self, ino: u64, path: &str) -> Option<FileAttr> {
        let p = self.root.join(path).ok()?;
        if !p.exists().ok()? {
            return None;
        }
        let m = p.metadata().ok()?;
        Some(self.make_attr(ino, m.file_type, m.len))
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }
}

// Internal helpers that work on owned/short-lived locals only — keeps
// borrow-checker noise out of each Filesystem method.

impl Filesystem for RspacefsFuse {
    // ── Lookup / metadata ────────────────────────────────────────────────────

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
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
        match p.exists() {
            Ok(true) => {}
            Ok(false) => return reply.error(ENOENT),
            Err(_) => return reply.error(EIO),
        }
        let meta = match p.metadata() {
            Ok(m) => m,
            Err(_) => return reply.error(EIO),
        };

        let ino = self.intern_path(path);
        let attr = self.make_attr(ino, meta.file_type, meta.len);
        reply.entry(&TTL, &attr, 0);
    }

    fn getattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: ReplyAttr,
    ) {
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
        let mut all: Vec<(u64, FileType, String)> =
            Vec::with_capacity(entries.len() + 2);
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
            let kind = match self.root.join(&child_path).and_then(|p| p.metadata()) {
                Ok(m) if m.file_type == VfsFileType::Directory => FileType::Directory,
                Ok(_) => FileType::RegularFile,
                Err(_) => FileType::RegularFile,
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
        let ino = self.intern_path(path);
        let attr = self.make_attr(ino, VfsFileType::Directory, 0);
        reply.entry(&TTL, &attr, 0);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
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
        let path = match self.path_of(ino) {
            Some(p) => p.to_string(),
            None => return reply.error(ENOENT),
        };
        let p = match self.root.join(&path) {
            Ok(v) => v,
            Err(_) => return reply.error(EIO),
        };

        let accmode = flags & libc::O_ACCMODE;
        let writable = accmode == libc::O_WRONLY || accmode == libc::O_RDWR;

        // Read full file content into the handle cache. Empty for write-only
        // O_TRUNC opens — kernel doesn't expect the prior content.
        let mut data = Vec::new();
        if !p.exists().unwrap_or(false) {
            return reply.error(ENOENT);
        }
        if (flags & libc::O_TRUNC) == 0 {
            if let Ok(mut f) = p.open_file() {
                if f.read_to_end(&mut data).is_err() {
                    return reply.error(EIO);
                }
            }
        }

        let fh = self.alloc_fh();
        self.open_files.insert(
            fh,
            OpenFile {
                path,
                data,
                dirty: false,
                writable,
            },
        );
        reply.opened(fh, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(file) = self.open_files.get(&fh) else {
            return reply.error(EBADF);
        };
        let start = offset as usize;
        if start >= file.data.len() {
            return reply.data(&[]);
        }
        let end = (start + size as usize).min(file.data.len());
        reply.data(&file.data[start..end]);
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(file) = self.open_files.get_mut(&fh) else {
            return reply.error(EBADF);
        };
        if !file.writable {
            return reply.error(libc::EACCES);
        }
        let off = offset as usize;
        let end = off + data.len();
        if end > file.data.len() {
            file.data.resize(end, 0);
        }
        file.data[off..end].copy_from_slice(data);
        file.dirty = true;
        reply.written(data.len() as u32);
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let Some(file) = self.open_files.remove(&fh) else {
            return reply.error(EBADF);
        };
        if file.dirty {
            let p = match self.root.join(&file.path) {
                Ok(v) => v,
                Err(_) => return reply.error(EIO),
            };
            let mut w = match p.create_file() {
                Ok(w) => w,
                Err(_) => return reply.error(EIO),
            };
            if w.write_all(&file.data).is_err() || w.flush().is_err() {
                return reply.error(EIO);
            }
        }
        reply.ok();
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
        let attr = self.make_attr(ino, VfsFileType::File, 0);
        let fh = self.alloc_fh();
        self.open_files.insert(
            fh,
            OpenFile {
                path,
                data: Vec::new(),
                dirty: false,
                writable: true,
            },
        );
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
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

    // ── Statfs ──────────────────────────────────────────────────────────────

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
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

// Silence the unused-import lint on platforms where it isn't pulled in.
const _: fn(&OsString) = |_| {};
