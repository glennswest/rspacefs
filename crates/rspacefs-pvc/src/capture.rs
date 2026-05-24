//! `capture_layer` — snapshot a `PvcMount`'s current upper into a
//! deterministic tar+zstd blob, plus its sha256 digest.
//!
//! Output is registry-pushable: hand the resulting tarball + digest to
//! `oras push` (or any OCI artifact pusher) and the next boot can pull
//! it back as a lower layer of a fresh `PvcMount` — workload-generated
//! "initial content" survives as a new revision.
//!
//! Determinism: we walk the upper in sorted order, emit tar entries with
//! fixed mtime=0, uid=0, gid=0, mode preserved from source. Same upper
//! tree always produces the same sha256, modulo zstd version (we pin
//! the compression level explicitly).

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::mount::PvcMount;

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Where to write the tarball. Parent dir must exist.
    pub out_path: PathBuf,
    /// zstd compression level (1..=22). Default 3 — fast, decent ratio.
    pub zstd_level: i32,
    /// Optional digest of a previous capture to compute a delta against.
    /// v0: ignored (always full capture). Reserved for v1's incremental
    /// snapshots so the API doesn't need a breaking change later.
    pub since: Option<String>,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            out_path: PathBuf::from("/tmp/rspacefs-pvc-capture.tar.zst"),
            zstd_level: 3,
            since: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureReport {
    pub out_path: PathBuf,
    /// `sha256:...` of the resulting tarball.
    pub digest: String,
    pub bytes_compressed: u64,
    pub entries: usize,
}

/// Snapshot `pvc.upper()` into a tar+zstd blob. Returns the resulting
/// path, digest, size, and entry count.
///
/// **Requires** `pvc.upper_physical()` to be set — captures walk the
/// real on-disk dir under the upper, not the merged view. The merged
/// view would re-include lower content, which is wrong for a delta
/// snapshot. Capture only what the workload has actually written.
pub fn capture_layer(
    pvc: &PvcMount,
    opts: CaptureOptions,
) -> Result<CaptureReport, crate::PvcError> {
    let upper_dir = pvc.upper_physical().ok_or_else(|| {
        crate::PvcError::InvalidArgument(
            "capture_layer requires the PvcMount to have upper_physical set".to_string(),
        )
    })?;

    // Open output + zstd encoder.
    let out = std::fs::File::create(&opts.out_path)?;
    let zstd_enc = zstd::Encoder::new(out, opts.zstd_level)?.auto_finish();
    // sha256-hash the compressed bytes as we write them.
    let hasher = HashingWriter::new(zstd_enc);
    let mut tar_builder = tar::Builder::new(hasher);
    tar_builder.mode(tar::HeaderMode::Deterministic);

    let mut entries = 0;
    walk_and_append(&mut tar_builder, upper_dir, upper_dir, &mut entries)?;
    let hashing = tar_builder.into_inner()?;
    let (digest_hex, total) = hashing.finalize();

    Ok(CaptureReport {
        out_path: opts.out_path,
        digest: format!("sha256:{}", digest_hex),
        bytes_compressed: total,
        entries,
    })
}

/// Walk `dir` recursively, append each entry to `tar_builder` with a
/// path relative to `base`. Sorted by filename for determinism.
fn walk_and_append<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    base: &Path,
    dir: &Path,
    entries: &mut usize,
) -> Result<(), crate::PvcError> {
    let mut children: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let path = child.path();
        let rel = path
            .strip_prefix(base)
            .map_err(|e| crate::PvcError::InvalidArgument(e.to_string()))?;
        let ft = child.file_type()?;
        if ft.is_dir() {
            // Append the dir entry itself (preserves mode) then recurse.
            tar_builder.append_path_with_name(&path, rel)?;
            *entries += 1;
            walk_and_append(tar_builder, base, &path, entries)?;
        } else if ft.is_file() || ft.is_symlink() {
            tar_builder.append_path_with_name(&path, rel)?;
            *entries += 1;
        }
        // Other types (sockets, fifos, devices) are skipped — PVCs are
        // content-only, not block devices.
    }
    Ok(())
}

/// Wraps a writer so every byte written is fed into a sha256 hasher.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    total: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            total: 0,
        }
    }
    fn finalize(self) -> (String, u64) {
        let hex = format!("{:x}", self.hasher.finalize());
        (hex, self.total)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.total += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::{PvcAccessMode, PvcLifecycle, PvcMount, PvcOptions};
    use std::io::Write;
    use tempfile::tempdir;
    use vfs::{PhysicalFS, VfsPath};

    #[test]
    fn capture_roundtrip_produces_deterministic_digest() {
        // Two captures of the same content should produce the same sha256.
        let work = tempdir().unwrap();
        let upper = work.path().join("upper");
        std::fs::create_dir_all(&upper).unwrap();
        // Write some files.
        std::fs::create_dir_all(upper.join("etc")).unwrap();
        let mut f = std::fs::File::create(upper.join("etc/cfg")).unwrap();
        f.write_all(b"hello").unwrap();
        let mut f = std::fs::File::create(upper.join("readme")).unwrap();
        f.write_all(b"world").unwrap();

        let upper_vfs = VfsPath::new(PhysicalFS::new(upper.clone()));
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::Empty,
            lifecycle: PvcLifecycle::Persistent,
            name: "test".into(),
            upper: upper_vfs,
            lowers: vec![],
            owner: None,
            upper_physical: Some(upper.clone()),
        })
        .unwrap();

        let r1 = capture_layer(
            &pvc,
            CaptureOptions {
                out_path: work.path().join("a.tar.zst"),
                ..Default::default()
            },
        )
        .expect("first capture");
        let r2 = capture_layer(
            &pvc,
            CaptureOptions {
                out_path: work.path().join("b.tar.zst"),
                ..Default::default()
            },
        )
        .expect("second capture");

        assert_eq!(
            r1.digest, r2.digest,
            "captures of the same upper must be byte-identical"
        );
        assert_eq!(r1.entries, r2.entries);
        assert!(r1.entries >= 3, "etc dir + 2 files = at least 3 entries");
    }
}
