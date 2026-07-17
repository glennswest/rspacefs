//! Integration test for the PVC lifecycle the boot agent drives:
//!
//!   empty PVC → workload writes → capture → (push/pull elided) →
//!   apply blob → second boot mounts the capture as a lower →
//!   previously-generated content is visible read-only, new writes
//!   land in the fresh upper.
//!
//! Everything through the public `rspacefs-pvc` API, no FUSE, no
//! daemon — this is the exact usage qregistry / rspaced link against.

use std::io::Write as _;

use rspacefs_pvc::{
    apply_blob, capture_layer, CaptureOptions, PvcAccessMode, PvcLifecycle, PvcMount, PvcOptions,
};
use vfs::{PhysicalFS, VfsPath};

#[test]
fn empty_pvc_capture_reboot_roundtrip() {
    let work = tempfile::tempdir().unwrap();

    // ── Boot 1: empty PVC, workload generates initial content ──────
    let upper1 = work.path().join("boot1-upper");
    std::fs::create_dir_all(&upper1).unwrap();
    let pvc1 = PvcMount::new(PvcOptions {
        access_mode: PvcAccessMode::Empty,
        lifecycle: PvcLifecycle::Persistent,
        name: "db-init".into(),
        upper: VfsPath::new(PhysicalFS::new(upper1.clone())),
        lowers: vec![],
        owner: Some((1000, 1000)),
        upper_physical: Some(upper1.clone()),
    })
    .expect("boot-1 empty PVC");

    // Workload writes through the merged view.
    pvc1.merged().join("schema").unwrap().create_dir().unwrap();
    pvc1.merged()
        .join("schema/init.sql")
        .unwrap()
        .create_file()
        .unwrap()
        .write_all(b"CREATE TABLE t (id INT);")
        .unwrap();
    pvc1.merged()
        .join("VERSION")
        .unwrap()
        .create_file()
        .unwrap()
        .write_all(b"rev1")
        .unwrap();

    // ── Capture the generated content as a registry-pushable layer ──
    let blob = work.path().join("db-init.tar.zst");
    let report = capture_layer(
        &pvc1,
        CaptureOptions {
            out_path: blob.clone(),
            ..Default::default()
        },
    )
    .expect("capture boot-1 upper");
    assert!(report.digest.starts_with("sha256:"));
    assert_eq!(report.entries, 3, "schema dir + init.sql + VERSION");

    // Determinism: capturing again yields the identical digest.
    let report2 = capture_layer(
        &pvc1,
        CaptureOptions {
            out_path: work.path().join("again.tar.zst"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.digest, report2.digest);

    // ── "Pull" the layer: extract the blob like the boot agent would ─
    let seed = work.path().join("pulled-seed");
    let applied = apply_blob(&blob, &seed).expect("extract pulled blob");
    assert_eq!(applied.entries, report.entries);

    // ── Boot 2: fresh PVC with the capture as lower ─────────────────
    let upper2 = work.path().join("boot2-upper");
    std::fs::create_dir_all(&upper2).unwrap();
    let pvc2 = PvcMount::new(PvcOptions {
        access_mode: PvcAccessMode::ReadWriteOnce,
        lifecycle: PvcLifecycle::Persistent,
        name: "db".into(),
        upper: VfsPath::new(PhysicalFS::new(upper2.clone())),
        lowers: vec![VfsPath::new(PhysicalFS::new(seed))],
        owner: Some((1000, 1000)),
        upper_physical: Some(upper2.clone()),
    })
    .expect("boot-2 seeded PVC");

    // Boot-1 content is visible through the merged view.
    let mut buf = String::new();
    pvc2.merged()
        .join("schema/init.sql")
        .unwrap()
        .open_file()
        .unwrap()
        .read_to_string(&mut buf)
        .unwrap();
    assert_eq!(buf, "CREATE TABLE t (id INT);");

    // New writes land in boot-2's upper, not the seed.
    pvc2.merged()
        .join("VERSION")
        .unwrap()
        .create_file()
        .unwrap()
        .write_all(b"rev2")
        .unwrap();
    assert_eq!(std::fs::read(upper2.join("VERSION")).unwrap(), b"rev2");

    // A second capture from boot-2's upper contains only the delta
    // (the rewritten VERSION), not the seed content.
    let delta = capture_layer(
        &pvc2,
        CaptureOptions {
            out_path: work.path().join("delta.tar.zst"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(delta.entries, 1, "only VERSION was written in boot 2");
}
