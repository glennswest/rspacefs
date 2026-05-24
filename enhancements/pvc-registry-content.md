# Enhancement: PVC-as-Registry-Content + Empty PVCs + Capture-to-Layer

Author: 2026-05-24.
Crate impact: `rspacefs-core`, `rspacefs-fuse`, `rspacefs-cli`.
Pairs with (but does not depend on) `qregistry/enhancements/pvc-content-type.md`
and `rspaced/enhancements/multi-registry-boot.md` — those describe the
**consumer**; this describes what rspacefs itself has to provide.

## Why

The boot path needs to mount a PVC where:

1. The content is **pulled from a registry as an OCI artifact** (per
   `qregistry/enhancements/pvc-content-type.md`), OR
2. The PVC is **empty** — no content, just a writable scratch area, OR
3. The PVC is **empty initially but accumulates "initial content"** that
   the workload generates on first run (think: cluster cert generation,
   database initial schema, model fine-tune output). That generated
   content can then be **captured as a new registry layer** so the next
   boot starts from it.

All three are the same rspacefs primitive — a LayerFS mount — with
different shapes of the lower stack and different decisions about whether
the upper is tmpfs (discarded on reboot) or persistent.

## What rspacefs has to add

### 1. `rspacefs-mount --pvc` mode (alongside `mount_program` and direct)

A new invocation form, distinct from the existing
`mount_program`-compatible mode. Where `mount_program` is for container
rootfs, `--pvc` is for explicit PVC mounts that the boot agent /
operator owns:

```
rspacefs-mount --pvc \
  --lower-blob /run/rspaced/pulled/sha256:abc…   # 0..N: the pulled PVC data
  --upper /run/rspaced/pvcs/<name>/upper          # tmpfs OR disk dir
  --workdir /run/rspaced/pvcs/<name>/work
  --control-socket /run/rspaced/pvcs/<name>.sock
  /var/lib/kubelet/pods/<uid>/volumes/.../<name>  # mountpoint
```

Differences from `mount_program` mode:
- Lower layers can be **zero** (empty PVC). `mount_program` requires
  ≥ 1 lowerdir; `--pvc` accepts an empty lower list.
- `--lower-blob` accepts a tarball or extracted directory — both work.
  A tarball is extracted into a per-mount cache dir on the first read
  (lazy) so a 10 GB PVC blob with sparse access doesn't force a 10 GB
  up-front extract.
- Mount path semantics match Kubernetes PVC expectations: mode 0755, a
  data-only mount (no special files), supports user/group ownership via
  `--owner uid:gid` for the lifetime of the mount.
- `allow_other` is on by default (same reasoning as mount_program — pods
  run under arbitrary UIDs).

### 2. Empty-PVC support in `LayerFS`

`LayerFS::new(upper, [])` — zero lowers — must compile and work. The
core resolution paths already handle "not found in any lower" gracefully;
the work is making sure:

- The whiteout cache doesn't trip on an empty lower vector.
- `readdir` on root returns only `upper`'s entries (no panic on
  `lowers.iter().next().unwrap()` patterns; none exist today but worth
  a test).
- Regression test: `test_zero_lower_layers_just_upper`.

### 3. Backing pivot (the load-bearing op for the boot flow)

A new control-socket command on a running `rspacefs-mount`:

```
{ "cmd": "pivot-upper",
  "new_upper": "/var/lib/rspaced/pvcs/<name>/upper",
  "preserve_open_files": true }
```

Semantics:
- Caller has already pre-populated `new_upper` (typically by
  reflink-snapshotting the current tmpfs upper to disk — the boot
  agent's job).
- Daemon pauses new FUSE ops on a global mutex.
- For every currently-open handle on the upper, do nothing — the
  file descriptors point at backing files whose inodes don't change;
  the kernel keeps reading the old tmpfs upper until the handle is
  closed. (This is correct only when `preserve_open_files=true` AND
  `new_upper` is a content-identical copy. The boot agent is
  responsible for the copy.)
- Atomically swap the `upper: VfsPath` in the `LayerFS` from the old
  tmpfs to `new_upper`.
- Resume FUSE ops. New opens go to the new upper.
- Return `{ "ok": true, "pivoted": true, "old_upper_in_use_by_handles": N }`.

The old tmpfs upper is **not** unmounted by rspacefs — the boot agent
holds the reference and tears down tmpfs once `N == 0` (queryable via
`stats`/`debug`).

This is the rspacefs equivalent of the rspaced "ephemeral-then-persistent"
promotion — rspacefs gives them the seam, rspaced uses it.

### 4. Capture-to-Layer

A new CLI / control-socket command that snapshots the **current upper**
of a running mount into a registry-pushable layer artifact:

```
rspacefs ctl --socket /run/rspaced/pvcs/<name>.sock \
  capture-layer \
  --out /var/lib/qregistry/uploads/<staging-id>.tar.zst \
  --since <last-snapshot-digest>     # optional, for incremental
```

Or via the daemon:

```
{ "cmd": "capture-layer",
  "out_path": "...",
  "since": "sha256:abc…" }
```

Semantics:
- Quiesce upper writes (control mutex + optional fsfreeze).
- Walk the upper tree.
- If `--since` is given, emit only entries whose mtime > the digest's
  recorded snapshot time AND whose content hash differs.
- Tar + zstd into `out_path`. Compute sha256 of the resulting tarball.
- Return the digest + size in the response.
- Resume writes.

The boot agent then pushes the resulting tarball via `oras push` to
qregistry as a new PVC layer. Subsequent boots can pull that layer as
the **lower**, giving the workload its previously-generated content for
free.

Capture is **content-only**, not block-level. PVC mode is for directory
trees, not raw block devices. Block-device PVCs (LVM, iSCSI) are out of
scope and stay with whatever provisioner already does them.

### 5. CLI subcommand to drive the round-trip

In `rspacefs-cli`, add a `pvc` subcommand family:

```
rspacefs pvc init     --upper DIR                    # empty PVC scaffold
rspacefs pvc apply    --upper DIR --blob TARBALL     # pre-stage from a pulled layer
rspacefs pvc capture  --upper DIR --out TARBALL      # offline capture (no daemon)
```

The offline variants are useful for `compose_rspaced` (build-time) and
debugging.

## Library shape — common crates for downstream consumers

qregistry, rspaced, and any future tooling will need the same primitives
(PVC mount, pivot upper, capture-to-layer). To avoid each project
re-implementing them, the primitives live in **library crates** that
expose pure-Rust APIs; `rspacefs-mount` is the FUSE daemon **frontend**
on top of those libraries.

### Proposed crate split (additive to today's layout)

| Crate | Existing? | Role | Consumers |
|---|---|---|---|
| `rspacefs-core` | ✅ today | `LayerFS` over `vfs::FileSystem`. Whiteouts, copy-up. Pure logic, no I/O policy. | everything |
| `rspacefs-verity` | ✅ today | SHA-256 Merkle tree + manifest + `VerifiedFS`. | everything |
| `rspacefs-pvc` | **new (library)** | PVC primitives: empty-layer mount setup, **`pivot_upper`**, **`capture_layer`**, deterministic-tar emitter, lazy-extract-from-blob, ownership/uid handling. Operates against an `&mut LayerFS` plus a small `PvcConfig`. **No daemon, no FUSE, no HTTP.** | rspacefs-fuse, qregistry, rspaced |
| `rspacefs-fuse` | ✅ today | FUSE daemon. Adds `--pvc` argv mode that constructs a `rspacefs_pvc::PvcMount` and exposes its ops over the control socket (`pivot-upper`, `capture-layer`). | end-users / boot agent |
| `rspacefs-cli` | ✅ today | `rspacefs pvc {init,apply,capture}`. Offline / scripted use; thin wrapper around `rspacefs-pvc`. | dev, build tooling |

The key contract: **everything in `rspacefs-pvc` is a `pub fn` callable
from a library consumer.** A project like qregistry can `cargo add
rspacefs-pvc` (path or crates.io dep) and call `capture_layer(&mut
layer_fs, opts) -> CaptureReport` directly from its Rust code — no
FUSE, no separate process, no IPC — for cases where qregistry itself
holds the LayerFS handle.

### Why qregistry needs the library form specifically

qregistry already runs **one `rspacefs-mount` daemon per tenant repo**.
For PVCs it could spawn yet another daemon per PVC, but the more
sensible shape is:
- The registry stays the OCI surface (push/pull blobs + manifests).
- When qregistry itself wants to **introspect** a PVC artifact (e.g.,
  the UI lists files inside a PVC; or a "diff between two captures"
  view; or server-side GC across captured-layer chains), it links
  `rspacefs-pvc` directly and operates in-process. No detour through
  FUSE.

The rspaced agent uses the FUSE daemon (because the kernel needs a real
mount); qregistry uses the library (because it's just inspecting bytes).
Same code path either way.

### Runtime support — a single shared daemon binary, optional

A bonus deliverable: a small `rspacefsd` binary that wraps the library
+ a JSON-over-Unix-socket control surface. It's just `rspacefs-mount`
without the FUSE-mount step; useful when a consumer needs the daemon
semantics (long-lived state, multiple PVCs, control-socket ops) but
*not* the kernel mount. qregistry-with-many-PVCs is the obvious user.
Out of scope for v1 of this enhancement; flagged here so the crate
boundary doesn't preclude it.

### Versioning & API stability

`rspacefs-pvc` follows the workspace's semver. Pre-1.0 the surface can
change between minor versions with a CHANGELOG note. Downstream
consumers (qregistry, rspaced) pin to a path dep or a tagged git ref
until 1.0; after 1.0 they pin to a minor.

### Acceptance for the library-shape goal

- [ ] New crate `crates/rspacefs-pvc/` exists in the workspace, builds,
  has unit tests.
- [ ] `rspacefs-fuse` depends on `rspacefs-pvc` for the new `--pvc`
  mode and the new control ops — no logic duplication.
- [ ] `rspacefs-cli` depends on `rspacefs-pvc` for the `pvc` subcommand
  family — no logic duplication.
- [ ] A short integration test in `crates/rspacefs-pvc/tests/` builds
  an in-memory LayerFS, calls `capture_layer`, parses the resulting
  tarball, and verifies the file set — proves the API is usable
  without any of the daemon scaffolding.
- [ ] `docs/library-api.md` documents the public surface for downstream
  projects.

## What stays out of scope

- **Mounting block-device PVCs.** Directory-tree only.
- **Multi-writer PVCs (RWX).** One mount, one writer. rspacefs is not a
  cluster FS.
- **Push to qregistry.** rspacefs hands a tarball + digest to the boot
  agent; the boot agent does the OCI push.
- **PVC manifest semantics.** The artifactType, annotations, and
  lifecycle values are qregistry's contract — see
  `qregistry/enhancements/pvc-content-type.md`. rspacefs just provides
  the mount + capture primitives.

## Acceptance

- [ ] `LayerFS::new(upper, vec![])` works; regression test added
- [ ] `rspacefs-mount --pvc` mounts an empty PVC, supports writes, mode
  bits + ownership correct under non-root uids
- [ ] `rspacefs-mount --pvc --lower-blob <tarball>` lazily extracts +
  serves the contents through the merged view
- [ ] Control-socket `pivot-upper` atomically swaps the upper without
  errors on in-flight ops; the gauge `rspacefs_pivots_total` increments
- [ ] Control-socket `capture-layer` produces a deterministic tarball
  whose sha256 matches the digest reported in the response; round-trip
  via `tar -tzf` shows the expected file set
- [ ] `rspacefs pvc init|apply|capture` round-trip in the CLI
- [ ] All five subcommands documented in `docs/pvc.md` with a worked
  example matching the boot agent's expected sequence

## Test plan

Unit (`crates/rspacefs-core/`):
- empty-lower LayerFS basic ops
- pivot-upper with no open handles
- pivot-upper while a streaming-read handle is open (verifies handle
  survives the swap)
- capture-layer with a few files; check tarball sha256 reproducibility

Integration (`tests/k8s/workloads/pvc/`):
- Spin up a PVC mount on test3
- Write content via a pod
- Capture the upper to a tarball
- Push the tarball as a new qregistry artifact (manual `oras push` is
  fine for v1)
- Spin up a second PVC mount from the captured artifact as lower; verify
  content is visible
- Cycle: empty PVC → generate content → capture → boot with that as
  lower → second-boot content matches

Operational:
- The beatup test (`tests/k8s/workloads/beatup.sh`) should grow a PVC
  variant: spawn pods that write to a mounted PVC, capture, restart pod
  with the capture as lower.

## Stretch goals (separate enhancements later)

- **Layered captures.** Capture a delta against a previous capture
  (`--since`) so a registry can keep an append-only chain of PVC
  revisions cheaply.
- **fs-verity on captured layers.** Build a `LayerManifest` (see
  `rspacefs-verity`) alongside the tar, signed at capture time, so the
  consumer can verify before mount.
- **GC integration.** When a captured PVC layer is older than its
  promotion target, rspacefs marks it for sweep on the next qregistry GC.
  Coordinated via a small control-socket op so the boot agent knows
  what's safe to delete.

## Cross-references

- `qregistry/enhancements/pvc-content-type.md` — registry contract
- `rspaced/enhancements/multi-registry-boot.md` — boot agent that
  consumes this
- `crates/rspacefs-core/src/layer.rs` — `LayerFS` impl (already supports
  varying lower count; verify zero works)
- `crates/rspacefs-fuse/src/main.rs` — where `--pvc` mode goes
- `crates/rspacefs-fuse/src/control.rs` — where `pivot-upper` and
  `capture-layer` ops are added
