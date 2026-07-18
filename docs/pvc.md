# PVC mode — data PVCs served by rspacefs

`rspacefs-mount --pvc` mounts a PVC-shaped LayerFS: zero or more
read-only lowers (pulled registry artifacts or extracted directories)
under one writable upper (tmpfs or disk). It is the mount half of the
PVC-as-registry-content design in
`enhancements/pvc-registry-content.md`; the capture half turns whatever
the workload wrote back into a registry-pushable layer.

Design invariants:

- **Directory trees only.** Block-device PVCs stay with whatever
  provisioner already handles them.
- **One writer.** RWX at the rspacefs layer only relaxes FUSE-side
  access; cross-writer coordination is the caller's problem.
- **Capture is content-only and deterministic** — same upper tree, same
  sha256, so a registry can dedupe revisions.

## Mounting

```sh
rspacefs-mount --pvc \
  --name mydata \
  --access-mode rwo \
  --lifecycle persistent \
  --lower-blob /run/rspaced/pulled/sha256-abc.tar.zst \
  --upper /var/lib/rspaced/pvcs/mydata/upper \
  --owner 1000:1000 \
  --control-socket /run/rspaced/pvcs/mydata.sock \
  /var/lib/kubelet/pods/<uid>/volumes/rspacefs~pvc/mydata
```

- `--lower-blob` is repeatable, ordered top-down, and accepts either an
  extracted directory (used as-is) or a tar / tar+zstd blob (extracted
  into `$XDG_RUNTIME_DIR/rspacefs/` at mount time; lazy per-read
  extraction is a planned follow-up). Zero `--lower-blob`s = empty PVC.
- `--access-mode`: `empty` | `ro` | `rwo` (default) | `rwx`. `ro` also
  mounts the FUSE filesystem read-only.
- `--lifecycle`: `persistent` (default) | `ephemeral` |
  `ephemeral-then-persistent` (upper starts on tmpfs, later promoted to
  disk with `pivot-upper`).
- `--owner UID:GID` chowns the mount root to the workload's runAsUser.
- `allow_other` is always on (pods run under arbitrary UIDs).
- The process daemonizes once the kernel acknowledges the mount
  (`--foreground` to keep it attached).

## Live control ops (PVC mounts only)

Both require `--control-socket`. They ride the same newline-delimited
JSON protocol as `status` / `stats` / `invalidate`.

### pivot-upper — promote tmpfs → disk

The boot agent pre-populates the new upper (reflink/copy of the current
tmpfs contents), then:

```sh
rspacefs ctl --socket /run/rspaced/pvcs/mydata.sock \
  pivot-upper --new-upper /var/lib/rspaced/pvcs/mydata/upper
```

Response:

```json
{ "ok": true, "data": { "pivoted": true,
    "old_upper_in_use_by_handles": 3, "entries_invalidated": 7 } }
```

Requires lifecycle `ephemeral-then-persistent`. Open handles keep
reading the old tmpfs backing until they close; poll `rspacefs ctl ...
debug` (`open_handles`) before tearing the tmpfs down. After the swap
the daemon invalidates the kernel's top-level dentries so new lookups
resolve through the new upper. The `rspacefs_pivots_total` metric
increments.

### capture-layer — snapshot the upper into a registry artifact

```sh
rspacefs ctl --socket /run/rspaced/pvcs/mydata.sock \
  capture-layer --out /var/lib/qregistry/uploads/mydata-rev2.tar.zst
```

Response carries `digest` (`sha256:...` of the tarball), size, and
entry count. Captures only what the workload actually wrote (the upper),
never lower content. `--since` is accepted and reserved for incremental
captures (v0 always captures in full). The boot agent pushes the blob
(e.g. `oras push`) as the next PVC revision.

## Offline tools (no daemon)

```sh
rspacefs pvc init    --upper ./upper                 # scaffold an empty PVC
rspacefs pvc apply   --upper ./upper --blob seed.tar.zst   # pre-stage from a pulled layer
rspacefs pvc capture --upper ./upper --out rev1.tar.zst    # capture without a mount
```

`capture` prints the same JSON report as the live `capture-layer` op.

## The full lifecycle, end to end

```sh
# Boot 1: empty PVC; workload generates initial content
rspacefs pvc init --upper /pvcs/db/upper
rspacefs-mount --pvc --access-mode empty --name db \
  --upper /pvcs/db/upper --control-socket /run/db.sock /mnt/db
# ... workload writes /mnt/db/schema/init.sql ...

# Freeze what it generated as a registry layer
rspacefs ctl --socket /run/db.sock capture-layer --out /uploads/db-rev1.tar.zst
# push db-rev1.tar.zst (oras push / qregistry)

# Boot 2: same PVC, seeded with rev1 as a lower
rspacefs-mount --pvc --access-mode rwo --name db \
  --lower-blob /pulled/db-rev1.tar.zst \
  --upper /pvcs/db/upper2 --control-socket /run/db.sock /mnt/db
# workload sees its own generated content; new writes land in upper2
```

## Library consumers

Everything above is a thin frontend over the `rspacefs-pvc` crate
(`PvcMount`, `pivot_upper`, `capture_layer`, `apply_blob`) — qregistry
and rspaced link it directly; see
`crates/rspacefs-pvc/tests/roundtrip.rs` for the API in one page.

## Kubernetes integration

Today the mount is owned by whoever invokes `rspacefs-mount --pvc`
(boot agent, operator, scripts). Native PVC provisioning via a CSI
driver (`rspacefs-csi`) is planned in its own repo:
<https://github.com/glennswest/rspacefs-csi>. Note this is orthogonal to
image rootfs mounts: CSI-provisioned volumes from other drivers already
work fine on top of rspacefs `mount_program` rootfs mounts.
