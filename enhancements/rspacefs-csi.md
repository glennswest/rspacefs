# Enhancement: rspacefs-csi — CSI driver for rspacefs data PVCs

Status: proposed (placeholder — requested 2026-07-17, not yet designed
in detail). Depends on `--pvc` mode
(`enhancements/pvc-registry-content.md`, implemented).

## Why

`rspacefs-mount --pvc` gives us registry-seeded, capturable data PVCs,
but today something outside Kubernetes (boot agent, operator scripts)
has to own the mount lifecycle. A CSI driver makes rspacefs PVCs
first-class Kubernetes objects: a `StorageClass` + PVC yaml, and the
kubelet drives mount/unmount through the standard CSI RPCs.

## Shape (sketch)

- **One binary, `rspacefs-csi`**, running as the usual two components:
  - *Controller plugin* (Deployment): `CreateVolume`/`DeleteVolume` —
    resolves a registry artifact reference from StorageClass/PVC
    parameters (e.g. `seed: qregistry.local/pvcs/db:rev1`), pulls the
    blob, allocates the upper dir.
  - *Node plugin* (DaemonSet): `NodePublishVolume` execs
    `rspacefs-mount --pvc --lower-blob <pulled> --upper <dir>
    --control-socket <sock> <target_path>`; `NodeUnpublishVolume`
    unmounts and reaps the daemon.
- **StorageClass parameters** map 1:1 onto `--pvc` flags: seed artifact
  (0..N), access mode, lifecycle, owner, capture-on-delete policy.
- **VolumeSnapshot support** maps onto `capture-layer` — a snapshot IS
  a captured registry artifact, which is the whole point: snapshots are
  pushable, pullable, and dedupe by digest.
- gRPC surface per CSI spec v1.x; identity service advertises
  `rspacefs.csi.g8.io`.

## Open questions

- Registry auth handoff (pull secrets → blob pull in the controller).
- Where captures get pushed on snapshot/delete (StorageClass param vs
  VolumeSnapshotClass param).
- RWX semantics across nodes — likely refuse (single-node FS), or
  topology-constrain to one node.

## Cross-references

- `docs/pvc.md` — the mount/control surface the node plugin drives
- `enhancements/pvc-registry-content.md` — PVC-as-registry-content design
- `enhancements/rspacefs-registry-head.md` — the registry that stores
  PVC revisions
