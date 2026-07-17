//! `apply_blob` — extract a pulled PVC blob into a directory.
//!
//! The boot agent / registry hands rspacefs a PVC layer either as an
//! already-extracted directory (used as-is) or as a tarball. This
//! module handles the tarball case: plain tar or tar+zstd (the format
//! `capture_layer` emits). Compression is sniffed from the file magic,
//! not the extension, so `oras pull` output works regardless of how it
//! was named.
//!
//! gzip blobs are rejected with a clear error for now — the capture →
//! push → pull round-trip inside the rspacefs/qregistry world is always
//! zstd, and adding a flate2 dependency for foreign blobs is a separate
//! decision.

use std::io::Read;
use std::path::{Path, PathBuf};

/// First 4 bytes of a zstd frame (little-endian 0xFD2FB528).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// First 2 bytes of a gzip stream.
const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub dest: PathBuf,
    pub entries: usize,
    /// Sum of the tar entries' uncompressed sizes.
    pub bytes_written: u64,
}

/// Extract `blob` (plain tar or tar+zstd) into `dest`, creating `dest`
/// if needed. Uses `tar`'s `unpack_in`, which rejects path-traversal
/// entries (`..`, absolute paths) instead of following them.
pub fn apply_blob(blob: &Path, dest: &Path) -> Result<ApplyReport, crate::PvcError> {
    let mut f = std::fs::File::open(blob)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    // Re-open rather than seek so the decoder sees the stream from byte 0
    // regardless of reader internals.
    drop(f);
    let f = std::fs::File::open(blob)?;

    let reader: Box<dyn Read> = if n >= 4 && magic == ZSTD_MAGIC {
        Box::new(zstd::Decoder::new(f)?)
    } else if n >= 2 && magic[..2] == GZIP_MAGIC {
        return Err(crate::PvcError::InvalidArgument(format!(
            "{} is gzip-compressed; PVC blobs are tar or tar+zstd (recompress with \
             `zcat blob | zstd -o blob.tar.zst`)",
            blob.display()
        )));
    } else {
        Box::new(f)
    };

    std::fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);

    let mut entries = 0usize;
    let mut bytes_written = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        bytes_written += entry.header().size().unwrap_or(0);
        if !entry.unpack_in(dest)? {
            return Err(crate::PvcError::InvalidArgument(format!(
                "blob entry {:?} escapes the destination directory",
                entry.path().unwrap_or_default()
            )));
        }
        entries += 1;
    }

    Ok(ApplyReport {
        dest: dest.to_path_buf(),
        entries,
        bytes_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{capture_layer, CaptureOptions};
    use crate::mount::{PvcAccessMode, PvcLifecycle, PvcMount, PvcOptions};
    use std::io::Write;
    use tempfile::tempdir;
    use vfs::{PhysicalFS, VfsPath};

    #[test]
    fn apply_extracts_a_capture_tarball() {
        let work = tempdir().unwrap();
        let upper = work.path().join("upper");
        std::fs::create_dir_all(upper.join("data")).unwrap();
        std::fs::File::create(upper.join("data/one"))
            .unwrap()
            .write_all(b"1")
            .unwrap();
        std::fs::File::create(upper.join("two"))
            .unwrap()
            .write_all(b"22")
            .unwrap();

        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::Empty,
            lifecycle: PvcLifecycle::Persistent,
            name: "blob-test".into(),
            upper: VfsPath::new(PhysicalFS::new(upper.clone())),
            lowers: vec![],
            owner: None,
            upper_physical: Some(upper),
        })
        .unwrap();
        let report = capture_layer(
            &pvc,
            CaptureOptions {
                out_path: work.path().join("c.tar.zst"),
                ..Default::default()
            },
        )
        .unwrap();

        let dest = work.path().join("extracted");
        let applied = apply_blob(&report.out_path, &dest).unwrap();
        assert_eq!(applied.entries, report.entries);
        assert_eq!(std::fs::read(dest.join("data/one")).unwrap(), b"1");
        assert_eq!(std::fs::read(dest.join("two")).unwrap(), b"22");
    }

    #[test]
    fn apply_rejects_gzip() {
        let work = tempdir().unwrap();
        let blob = work.path().join("blob.tar.gz");
        std::fs::write(&blob, [0x1F, 0x8B, 0x08, 0x00]).unwrap();
        let r = apply_blob(&blob, &work.path().join("out"));
        assert!(matches!(r, Err(crate::PvcError::InvalidArgument(_))));
    }
}
