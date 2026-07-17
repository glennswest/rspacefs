//! `SwappableRoot` — a `vfs::FileSystem` that delegates every operation
//! to an inner root `VfsPath` held behind an `RwLock`.
//!
//! This is the seam that makes `pivot_upper` work on a *live* FUSE
//! mount: the FUSE adapter holds a clone of the merged `VfsPath` for the
//! lifetime of the mount, and a `VfsPath` clone pins whatever
//! `FileSystem` it was created over. If the merged view were a bare
//! `LayerFS`, swapping the upper would rebuild a new `VfsPath` that the
//! already-running FUSE session never sees. With `SwappableRoot` in
//! between, the daemon's clone stays fixed while the root underneath it
//! is replaced atomically; the next operation resolves through the new
//! upper.
//!
//! Open file handles obtained *before* a swap keep reading their old
//! backing — they hold boxed readers/writers from the previous
//! `LayerFS`, which stays alive until the last one drops. That is
//! exactly the `preserve_open_files` contract in
//! `enhancements/pvc-registry-content.md`.

use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use vfs::error::VfsErrorKind;
use vfs::{FileSystem, SeekAndRead, SeekAndWrite, VfsError, VfsMetadata, VfsPath, VfsResult};

/// Shared handle to the current inner root. `PvcMount` keeps one clone
/// so `pivot_upper` can write a new root; the `SwappableRoot` FS keeps
/// the other and reads it on every operation.
pub(crate) type RootHandle = Arc<RwLock<VfsPath>>;

pub(crate) struct SwappableRoot {
    inner: RootHandle,
}

impl SwappableRoot {
    pub(crate) fn new(inner: RootHandle) -> Self {
        Self { inner }
    }

    fn at(&self, path: &str) -> VfsResult<VfsPath> {
        let root = self
            .inner
            .read()
            .map_err(|_| {
                VfsError::from(VfsErrorKind::Other("swappable root lock poisoned".into()))
            })?
            .clone();
        let rel = path.trim_start_matches('/');
        if rel.is_empty() {
            Ok(root)
        } else {
            root.join(rel)
        }
    }
}

impl std::fmt::Debug for SwappableRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let root = self.inner.read().map(|p| p.as_str().to_string());
        f.debug_struct("SwappableRoot")
            .field("root", &root.unwrap_or_else(|_| "<poisoned>".into()))
            .finish()
    }
}

impl FileSystem for SwappableRoot {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let names: Vec<String> = self.at(path)?.read_dir()?.map(|e| e.filename()).collect();
        Ok(Box::new(names.into_iter()))
    }
    fn create_dir(&self, path: &str) -> VfsResult<()> {
        self.at(path)?.create_dir()
    }
    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        self.at(path)?.open_file()
    }
    fn create_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        self.at(path)?.create_file()
    }
    fn append_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        self.at(path)?.append_file()
    }
    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        self.at(path)?.metadata()
    }
    fn set_creation_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.at(path)?.set_creation_time(time)
    }
    fn set_modification_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.at(path)?.set_modification_time(time)
    }
    fn set_access_time(&self, path: &str, time: SystemTime) -> VfsResult<()> {
        self.at(path)?.set_access_time(time)
    }
    fn exists(&self, path: &str) -> VfsResult<bool> {
        self.at(path)?.exists()
    }
    fn remove_file(&self, path: &str) -> VfsResult<()> {
        self.at(path)?.remove_file()
    }
    fn remove_dir(&self, path: &str) -> VfsResult<()> {
        self.at(path)?.remove_dir()
    }
    fn copy_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.at(src)?.copy_file(&self.at(dest)?)
    }
    fn move_file(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.at(src)?.move_file(&self.at(dest)?)
    }
    fn move_dir(&self, src: &str, dest: &str) -> VfsResult<()> {
        self.at(src)?.move_dir(&self.at(dest)?)
    }
}
