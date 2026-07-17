//! `pivot_upper` — atomically swap the upper layer of a live `PvcMount`.
//!
//! Used by `rspaced` to promote an "ephemeral-then-persistent" PVC from
//! its tmpfs upper to a disk-backed upper after boot stabilises. The
//! caller is responsible for ensuring the new upper is content-identical
//! to the old one at the swap instant (typically by reflink-snapshotting
//! tmpfs → disk just before calling here).
//!
//! What this function does NOT do:
//!
//! - It does NOT unmount the old upper. The old VfsPath stays valid
//!   for any FUSE handles that were open at swap time — the kernel
//!   already holds backing fds against the old inodes. The caller's
//!   responsibility is to keep the old upper alive until those handles
//!   close.
//! - It does NOT verify byte-identity between old and new upper. The
//!   contract is "caller guarantees identity"; we trust them and swap.
//! - It does NOT freeze the filesystem. The FUSE daemon's control
//!   thread is expected to serialise calls into this function via its
//!   own mutex; this function itself isn't FUSE-aware.

use rspacefs_core::LayerFS;
use vfs::VfsPath;

use crate::mount::{PvcAccessMode, PvcMount};
// Bring lifecycle into scope through a separate use so the diff stays
// surgical.
use crate::mount::PvcLifecycle;

#[derive(Debug, Clone)]
pub struct PivotReport {
    pub old_upper_kept: bool,
    /// Number of FUSE handles the caller (typically the daemon) reports
    /// are still attached to the old upper. We don't read this from the
    /// kernel; the caller passes it in (or omits and gets None).
    pub handles_on_old_upper: Option<usize>,
}

/// Swap the upper of `pvc` from its current backing to `new_upper`.
/// Returns when the swap is complete; the old upper VfsPath is dropped
/// by `pvc` but the caller is responsible for any underlying tmpfs /
/// directory teardown.
pub fn pivot_upper(
    pvc: &mut PvcMount,
    new_upper: VfsPath,
    new_upper_physical: Option<std::path::PathBuf>,
    handles_on_old_upper: Option<usize>,
) -> Result<PivotReport, crate::PvcError> {
    // Refuse to pivot a ReadOnly PVC — by definition it has no writable
    // upper to swap.
    if pvc.access_mode == PvcAccessMode::ReadOnly {
        return Err(crate::PvcError::InvalidArgument(
            "pivot_upper not allowed on a ReadOnly PVC".into(),
        ));
    }
    // Only EphemeralThenPersistent makes pivot meaningful — it's the
    // "promote tmpfs to disk" operation. Persistent PVCs have nothing
    // to promote (already on disk); pure Ephemeral PVCs have nothing
    // worth promoting (caller chose to discard writes).
    if pvc.lifecycle != PvcLifecycle::EphemeralThenPersistent {
        return Err(crate::PvcError::InvalidArgument(format!(
            "pivot_upper requires lifecycle EphemeralThenPersistent; got {:?}",
            pvc.lifecycle
        )));
    }

    // Validate writability of new_upper before we swap.
    let probe = new_upper
        .join(".rspacefs-pvc-pivot-probe")
        .map_err(crate::PvcError::Vfs)?;
    probe.create_dir().map_err(crate::PvcError::Vfs)?;
    probe.remove_dir().map_err(crate::PvcError::Vfs)?;

    // Rebuild the LayerFS with new_upper while preserving lowers, and
    // store it through the SwappableRoot handle. Every clone of
    // `pvc.merged()` — including one a FUSE adapter captured at mount
    // time — re-roots atomically onto the new upper. Readers/writers
    // opened before the swap keep their boxed handles into the old
    // LayerFS, which stays alive until the last one drops.
    let new_root = VfsPath::new(LayerFS::new(new_upper.clone(), pvc.lowers.clone()));
    {
        let mut guard = pvc.root_handle.write().map_err(|_| {
            crate::PvcError::InvalidArgument("pivot_upper: root handle lock poisoned".into())
        })?;
        *guard = new_root;
    }
    pvc.upper = new_upper;
    pvc.upper_physical = new_upper_physical;

    Ok(PivotReport {
        old_upper_kept: true,
        handles_on_old_upper,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::{PvcAccessMode, PvcLifecycle, PvcMount, PvcOptions};
    use vfs::{MemoryFS, VfsPath};

    fn mem() -> VfsPath {
        VfsPath::new(MemoryFS::new())
    }

    #[test]
    fn pivot_replaces_upper_preserves_lowers() {
        let upper_a = mem();
        let lower = mem();
        lower
            .join("seed.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"seed")
            .unwrap();
        let mut pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadWriteOnce,
            lifecycle: PvcLifecycle::EphemeralThenPersistent,
            name: "test".into(),
            upper: upper_a,
            lowers: vec![lower.clone()],
            owner: None,
            upper_physical: None,
        })
        .unwrap();

        // Write something into upper_a via the merged view.
        pvc.merged()
            .join("upper-only.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"in upper A")
            .unwrap();

        // Clone the merged view BEFORE the pivot — this simulates the
        // FUSE adapter, which captures the merged VfsPath at mount time
        // and never rebuilds it. The pivot must be visible through it.
        let daemon_view = pvc.merged().clone();

        // Pivot to upper_b (empty).
        let upper_b = mem();
        let report = pivot_upper(&mut pvc, upper_b.clone(), None, Some(0)).unwrap();
        assert!(report.old_upper_kept);

        // The pre-pivot clone re-roots too: seed.txt (lower) visible,
        // upper A's file gone.
        assert!(daemon_view.join("seed.txt").unwrap().exists().unwrap());
        assert!(!daemon_view
            .join("upper-only.txt")
            .unwrap()
            .exists()
            .unwrap());

        // Through the new merged view: lower's seed.txt still visible,
        // but upper-only.txt (which was in upper_a) is gone.
        assert!(pvc.merged().join("seed.txt").unwrap().exists().unwrap());
        assert!(!pvc
            .merged()
            .join("upper-only.txt")
            .unwrap()
            .exists()
            .unwrap());
    }
}
