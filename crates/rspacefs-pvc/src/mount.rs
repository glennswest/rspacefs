//! `PvcMount` — a `LayerFS` configured for PVC lifecycle, plus runtime
//! state the other ops (`pivot_upper`, `capture_layer`) read and update.
//!
//! Unlike the `mount_program` path (container rootfs, always lower+upper),
//! a PVC mount can have:
//!
//! - Zero lowers (empty PVC, just a writable scratch upper).
//! - One lower from a pulled registry layer (PVC with seed content).
//! - Multiple lowers (PVC with seed + intermediate captures stacked).
//!
//! And it carries:
//!
//! - The upper's current `VfsPath` (swappable via `pivot_upper`).
//! - Owner uid/gid (so non-root containers in user namespaces can read
//!   their own PVC).
//! - A name (cosmetic, used in logs and capture-output filenames).

use std::path::PathBuf;

use rspacefs_core::LayerFS;
use vfs::VfsPath;

/// PVC access mode — mirrors the Kubernetes PVC access mode taxonomy
/// but is enforced at the rspacefs layer (FUSE / library) rather than
/// at the kube-scheduler.
///
/// | Mode | Lowers | Upper writable | Notes |
/// |---|---|---|---|
/// | `Empty`           | 0     | yes | Scratch PVC — workload writes go to upper. |
/// | `ReadOnly`        | 1+    | no  | "Info" PVCs: pulled-from-registry content, never modified. Pivot/capture not allowed. |
/// | `ReadWriteOnce`   | 0..N  | yes | Normal PVC — seed content from lowers, writes accumulate in upper. Capture-able into a new layer. |
/// | `ReadWriteMany`   | 0..N  | yes | Same FS semantics as RWO at the rspacefs layer (FUSE `allow_other` already lets multiple readers in). External coordination is the caller's problem. Single-node only — rspacefs is not a cluster FS. |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvcAccessMode {
    Empty,
    ReadOnly,
    ReadWriteOnce,
    ReadWriteMany,
}

impl PvcAccessMode {
    /// True if the workload can write to this PVC.
    pub fn writable(&self) -> bool {
        matches!(
            self,
            Self::Empty | Self::ReadWriteOnce | Self::ReadWriteMany
        )
    }
    /// True if `pivot_upper` / `capture_layer` make sense on this PVC.
    /// (ReadOnly has nothing to pivot, nothing to capture — refuse.)
    pub fn supports_capture(&self) -> bool {
        self.writable()
    }
}

/// PVC lifecycle — orthogonal to access mode. Controls what happens to
/// writes between mounts.
///
/// | Mode | Writes survive reboot? | `pivot_upper` allowed? | `capture_layer` allowed? |
/// |---|---|---|---|
/// | `Persistent` | yes — upper is disk-backed from the start | no (no need) | yes |
/// | `Ephemeral`  | no  — upper is tmpfs, discarded on unmount | no | yes — "freeze" tmpfs into a new registry-pushable layer |
/// | `EphemeralThenPersistent` | initially no, after promotion yes | yes — promotion swaps tmpfs upper to disk upper | yes |
///
/// The boot path's typical case for an RWO/RWMany PVC coming from
/// read-only boot media (ISO, signed bootc artifact) is
/// `EphemeralThenPersistent`: start on tmpfs because we don't trust the
/// new install yet; once the cluster's healthy, promote the tmpfs to
/// disk so the next reboot keeps the workload-generated state.
///
/// Pure `Ephemeral` is for "scratch" PVCs — writes accumulate during
/// the boot, get discarded next time, and the workload always re-starts
/// from the original lower content. Useful for stateless services that
/// want a writable scratch dir but don't want to commit anything.
///
/// `capture_layer` works on every writable lifecycle — even pure
/// `Ephemeral`. That's the "freeze the scratch into a new layer" path:
/// run the workload, let it generate initial content, capture what it
/// produced as a new registry layer for next boot to use as `ReadOnly`
/// seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvcLifecycle {
    Persistent,
    Ephemeral,
    EphemeralThenPersistent,
}

impl PvcLifecycle {
    /// True if `pivot_upper` is allowed for this lifecycle.
    pub fn supports_pivot(&self) -> bool {
        matches!(self, Self::EphemeralThenPersistent)
    }
}

/// Options consumed by [`PvcMount::new`]. Path-shaped intentionally —
/// the caller decides whether each layer is a tmpfs dir, a `PhysicalFS`
/// over disk, or anything else `VfsPath`-able.
#[derive(Debug, Clone)]
pub struct PvcOptions {
    /// Access mode. Constrains the upper / lower shapes:
    /// - `Empty` requires `lowers.is_empty()` (constructor enforces).
    /// - `ReadOnly` requires `!lowers.is_empty()`; upper is still required
    ///   (some VfsPath, possibly tmpfs scratch) for LayerFS bookkeeping
    ///   but writes through the merged view get an EROFS-equivalent error.
    /// - `ReadWriteOnce` / `ReadWriteMany`: any lower count, upper writable.
    pub access_mode: PvcAccessMode,
    /// Lifecycle — what happens to upper-layer writes between mounts.
    /// See [`PvcLifecycle`] for the matrix. Defaults to `Persistent`
    /// for the common case; the boot agent / ISO path will explicitly
    /// pass `EphemeralThenPersistent`.
    pub lifecycle: PvcLifecycle,

    /// Cosmetic name for logs / capture-output filenames.
    pub name: String,
    /// The writable upper layer. Caller has already created the
    /// directory (tmpfs or disk).
    pub upper: VfsPath,
    /// Zero or more read-only lower layers. Order is top-down — first
    /// `lower` has the highest priority. Empty vec = empty PVC.
    pub lowers: Vec<VfsPath>,
    /// uid/gid the mount should report for files lacking a more specific
    /// owner (typically the workload's runAsUser). `None` falls back to
    /// the calling process's identity.
    pub owner: Option<(u32, u32)>,
    /// Where on disk the upper is rooted, for `pivot_upper`'s
    /// equality check + capture's tar root. Optional because some
    /// in-memory VfsPath impls don't correspond to a disk path.
    pub upper_physical: Option<PathBuf>,
}

/// A live PVC mount. Holds the `LayerFS` plus mutable runtime state.
///
/// Cheap to construct (no I/O). The actual FUSE mount is the FUSE
/// daemon's job; this struct just lets downstream consumers reason
/// about the layer set, swap the upper, or capture it without needing
/// to wire up FUSE.
pub struct PvcMount {
    pub(crate) name: String,
    pub(crate) access_mode: PvcAccessMode,
    pub(crate) lifecycle: PvcLifecycle,
    /// The merged view (LayerFS-wrapped). Consumers operate through
    /// this; `pivot_upper` replaces it with a fresh `LayerFS` over the
    /// new upper + same lowers.
    pub(crate) merged: VfsPath,
    pub(crate) upper: VfsPath,
    pub(crate) upper_physical: Option<PathBuf>,
    /// Mirror of the lowers handed to `LayerFS::new`. `LayerFS` itself
    /// doesn't expose its lower vec; we keep one here so `pivot_upper`
    /// can rebuild the `LayerFS` with the same lowers and a new upper.
    pub(crate) lowers: Vec<VfsPath>,
    /// Disk path for each lower, parallel to `lowers`. Not used today;
    /// kept for v1 incremental-capture-against-a-specific-lower-digest
    /// where we need to walk a lower's physical contents.
    #[allow(dead_code)]
    pub(crate) lowers_physical: Vec<Option<PathBuf>>,
    pub(crate) owner: Option<(u32, u32)>,
}

impl PvcMount {
    /// Build a fresh `PvcMount` from `opts`. Validates that `upper` is a
    /// writable VfsPath (it `create_dir` on a marker entry under it,
    /// then cleans up). Empty `lowers` is allowed.
    pub fn new(opts: PvcOptions) -> Result<Self, crate::PvcError> {
        let PvcOptions {
            access_mode,
            lifecycle,
            name,
            upper,
            lowers,
            owner,
            upper_physical,
        } = opts;

        // Lifecycle / access-mode compatibility:
        // ReadOnly has no writes to lose or preserve, so the only
        // meaningful lifecycle is Persistent. Non-Persistent on a
        // ReadOnly is a configuration mistake — refuse so callers
        // notice early instead of debugging "why doesn't my ephemeral
        // readonly do anything?".
        if access_mode == PvcAccessMode::ReadOnly && lifecycle != PvcLifecycle::Persistent {
            return Err(crate::PvcError::InvalidArgument(format!(
                "ReadOnly PVC must use Persistent lifecycle; got {:?}",
                lifecycle
            )));
        }

        // Shape constraints per access mode.
        match access_mode {
            PvcAccessMode::Empty => {
                if !lowers.is_empty() {
                    return Err(crate::PvcError::InvalidArgument(format!(
                        "PvcAccessMode::Empty requires zero lowers, got {}",
                        lowers.len()
                    )));
                }
            }
            PvcAccessMode::ReadOnly => {
                if lowers.is_empty() {
                    return Err(crate::PvcError::InvalidArgument(
                        "PvcAccessMode::ReadOnly requires at least one lower (the content)".into(),
                    ));
                }
            }
            PvcAccessMode::ReadWriteOnce | PvcAccessMode::ReadWriteMany => {
                // Any lower count is fine.
            }
        }

        // Validate writability of upper without leaving a turd — except
        // for ReadOnly, where the upper may itself be a read-only stub.
        if access_mode.writable() {
            let probe = upper
                .join(".rspacefs-pvc-probe")
                .map_err(crate::PvcError::Vfs)?;
            probe.create_dir().map_err(crate::PvcError::Vfs)?;
            probe.remove_dir().map_err(crate::PvcError::Vfs)?;
        }

        let lowers_physical = vec![None; lowers.len()];
        let merged = VfsPath::new(LayerFS::new(upper.clone(), lowers.clone()));
        Ok(Self {
            name,
            access_mode,
            lifecycle,
            merged,
            upper,
            upper_physical,
            lowers,
            lowers_physical,
            owner,
        })
    }

    pub fn access_mode(&self) -> PvcAccessMode {
        self.access_mode
    }
    pub fn lifecycle(&self) -> PvcLifecycle {
        self.lifecycle
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn upper(&self) -> &VfsPath {
        &self.upper
    }

    pub fn upper_physical(&self) -> Option<&std::path::Path> {
        self.upper_physical.as_deref()
    }

    pub fn owner(&self) -> Option<(u32, u32)> {
        self.owner
    }

    /// The merged-view `VfsPath` over upper + lowers. Consumers
    /// operate through this — read files, walk dirs, write into upper.
    /// Cheap to clone; opaque to whether the backing is rspacefs's
    /// `LayerFS`, a verity-wrapped layer, an in-memory FS, etc.
    pub fn merged(&self) -> &VfsPath {
        &self.merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::MemoryFS;

    fn mem() -> VfsPath {
        VfsPath::new(MemoryFS::new())
    }

    #[test]
    fn empty_pvc_constructs() {
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::Empty,
            lifecycle: PvcLifecycle::Persistent,
            name: "empty".into(),
            upper: mem(),
            lowers: vec![],
            owner: Some((1000, 1000)),
            upper_physical: None,
        })
        .expect("construct empty PVC");
        assert_eq!(pvc.name(), "empty");
        assert_eq!(pvc.owner(), Some((1000, 1000)));
        assert_eq!(pvc.access_mode(), PvcAccessMode::Empty);
        assert!(pvc.access_mode().writable());
    }

    #[test]
    fn empty_pvc_rejects_lowers() {
        let lower = mem();
        let r = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::Empty,
            lifecycle: PvcLifecycle::Persistent,
            name: "bad".into(),
            upper: mem(),
            lowers: vec![lower],
            owner: None,
            upper_physical: None,
        });
        assert!(matches!(r, Err(crate::PvcError::InvalidArgument(_))));
    }

    #[test]
    fn readonly_pvc_requires_lower() {
        let r = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadOnly,
            lifecycle: PvcLifecycle::Persistent,
            name: "info".into(),
            upper: mem(),
            lowers: vec![],
            owner: None,
            upper_physical: None,
        });
        assert!(matches!(r, Err(crate::PvcError::InvalidArgument(_))));
    }

    #[test]
    fn readonly_pvc_constructs_with_lower() {
        let lower = mem();
        lower
            .join("info.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"baked-in")
            .unwrap();
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadOnly,
            lifecycle: PvcLifecycle::Persistent,
            name: "info".into(),
            upper: mem(),
            lowers: vec![lower],
            owner: None,
            upper_physical: None,
        })
        .expect("construct read-only PVC");
        assert_eq!(pvc.access_mode(), PvcAccessMode::ReadOnly);
        assert!(!pvc.access_mode().writable());
        assert!(!pvc.access_mode().supports_capture());

        let mut buf = Vec::new();
        pvc.merged()
            .join("info.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"baked-in");
    }

    #[test]
    fn rwo_pvc_with_one_lower_constructs() {
        let upper = mem();
        let lower = mem();
        lower
            .join("seed.txt")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadWriteOnce,
            lifecycle: PvcLifecycle::Persistent,
            name: "seeded".into(),
            upper,
            lowers: vec![lower],
            owner: None,
            upper_physical: None,
        })
        .expect("construct seeded RWO PVC");
        // Read through the merged view — the seed should be visible at root.
        let mut buf = Vec::new();
        pvc.merged()
            .join("seed.txt")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"hello");
    }

    #[test]
    fn rwo_ephemeral_then_persistent_from_iso_constructs() {
        // The boot-from-ISO case: a read-only lower (the ISO content),
        // tmpfs upper, EphemeralThenPersistent lifecycle. Writes go to
        // tmpfs initially; once the drive is up, pivot_upper swaps tmpfs
        // → disk.
        let iso_content = mem();
        iso_content
            .join("seed")
            .unwrap()
            .create_file()
            .unwrap()
            .write_all(b"from iso")
            .unwrap();
        let tmpfs_upper = mem();
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadWriteOnce,
            lifecycle: PvcLifecycle::EphemeralThenPersistent,
            name: "from-iso".into(),
            upper: tmpfs_upper,
            lowers: vec![iso_content],
            owner: None,
            upper_physical: None,
        })
        .expect("construct ISO-sourced PVC");
        assert_eq!(pvc.lifecycle(), PvcLifecycle::EphemeralThenPersistent);
        assert!(pvc.lifecycle().supports_pivot());
        // Read sees ISO content.
        let mut buf = Vec::new();
        pvc.merged()
            .join("seed")
            .unwrap()
            .open_file()
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        assert_eq!(buf, b"from iso");
    }

    #[test]
    fn ephemeral_pvc_for_scratch_constructs() {
        // Pure ephemeral: writes accumulate in tmpfs, discarded next
        // boot. Useful for stateless scratch dirs.
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadWriteOnce,
            lifecycle: PvcLifecycle::Ephemeral,
            name: "scratch".into(),
            upper: mem(),
            lowers: vec![],
            owner: None,
            upper_physical: None,
        })
        .expect("construct ephemeral scratch");
        assert!(!pvc.lifecycle().supports_pivot());
        // capture_layer still works — that's the "freeze" path.
        assert!(pvc.access_mode().supports_capture());
    }

    #[test]
    fn readonly_rejects_non_persistent_lifecycle() {
        let lower = mem();
        lower.join("x").unwrap().create_file().unwrap();
        let r = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadOnly,
            lifecycle: PvcLifecycle::Ephemeral,
            name: "bad".into(),
            upper: mem(),
            lowers: vec![lower],
            owner: None,
            upper_physical: None,
        });
        assert!(matches!(r, Err(crate::PvcError::InvalidArgument(_))));
    }

    #[test]
    fn rwmany_pvc_accepts_zero_lowers_too() {
        // RWMany is just RWO at the rspacefs layer — caller coordinates.
        let pvc = PvcMount::new(PvcOptions {
            access_mode: PvcAccessMode::ReadWriteMany,
            lifecycle: PvcLifecycle::Persistent,
            name: "shared".into(),
            upper: mem(),
            lowers: vec![],
            owner: None,
            upper_physical: None,
        })
        .expect("construct RWMany PVC");
        assert_eq!(pvc.access_mode(), PvcAccessMode::ReadWriteMany);
        assert!(pvc.access_mode().writable());
    }
}
