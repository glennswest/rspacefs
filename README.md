# rspacefs

**Pure-Rust userspace OverlayFS + dm-verity. No kernel, no NFS, no async.**

`rspacefs` provides two building blocks for layered, integrity-verified
container rootfs assembly that previously required kernel modules:

| Crate              | Role                                                                                                     |
|--------------------|-----------------------------------------------------------------------------------------------------------|
| `rspacefs-core`    | Userspace OverlayFS — merge N read-only lower layers with a writable upper layer, OCI whiteouts, copy-up. |
| `rspacefs-verity`  | Userspace dm-verity — SHA-256 Merkle tree over 4 KB blocks, per-file manifest, verified-block cache.      |
| `rspacefs-cli`     | `rspacefs` command-line tool — `overlay ls/cat/stat`, `verity build/verify/inspect`.                      |
| `rspacefs-fuse`    | `rspacefs-mount` Linux daemon — exposes an overlay (with optional verified lowers) as a real FUSE mount. |

Both crates expose plain synchronous APIs on top of the
[`vfs`](https://crates.io/crates/vfs) trait, so you can plug in any backend
— real disk (`PhysicalFS`), in-memory (`MemoryFS`), S3, a custom block-storage
driver, anything implementing `vfs::FileSystem`.

This code was originally part of [`nextnfs`](https://github.com/glennswest/nextnfs),
which served the merged tree over NFS. **rspacefs is fully independent of
nextnfs.** No NFS, no protocol layer, no network on the hot path — call it
directly from your container runtime, image-builder, or build tool.

## Why

Layered-rootfs primitives shouldn't require:

- **A kernel module.** Kernel overlayfs needs CAP_SYS_ADMIN, rules out
  rootless and many embedded targets, and ties you to Linux.
- **An NFS hop.** A network protocol in the data path is unacceptable for
  cold-start container provisioning, image build, or any high-IOPS workload.
- **An async runtime.** Callers pick their own concurrency model. Most
  callers want straight blocking I/O against a real filesystem.

`rspacefs` is what's left when you strip all that out: ~1,000 LOC of
overlay logic, ~1,400 LOC of verity, no dependencies beyond `vfs`, `sha2`,
`serde`. Same semantics as kernel overlayfs and dm-verity, in user-space.

## Quick start

### Library — overlay only

```rust
use rspacefs_core::OverlayFS;
use vfs::{PhysicalFS, VfsPath};

let upper = VfsPath::new(PhysicalFS::new("/var/lib/myapp/upper".into()));
let base  = VfsPath::new(PhysicalFS::new("/var/lib/myapp/base".into()));
let app   = VfsPath::new(PhysicalFS::new("/var/lib/myapp/app".into()));

// Lower layers ordered top-down: index 0 = highest priority.
let root: VfsPath = OverlayFS::new(upper, vec![app, base]).into();

// Use `root` as a regular merged filesystem.
```

### Library — verified read-only layer beneath an overlay

```rust
use rspacefs_core::OverlayFS;
use rspacefs_verity::{VerifiedFS, OnFailure};
use vfs::{MemoryFS, PhysicalFS, VfsPath};

let base = VfsPath::new(PhysicalFS::new("/var/lib/myapp/base".into()));
let base_verified: VfsPath =
    VerifiedFS::build(base, OnFailure::Reject).unwrap().into();

let upper: VfsPath = MemoryFS::new().into();
let root: VfsPath = OverlayFS::new(upper, vec![base_verified]).into();

// Reads through `root` are tamper-evident. Writes go to `upper`.
```

### CLI — inspect / build / verify

```sh
# Inspect a merged overlay
rspacefs overlay ls --upper ./upper --lower ./app --lower ./base /etc

rspacefs overlay cat \
  --upper ./upper --lower ./app --lower ./base /etc/passwd

# Build a verity manifest for a read-only layer
rspacefs verity build ./base --manifest base.manifest.json --tree base.tree

# Verify it later
rspacefs verity verify ./base --manifest base.manifest.json

# Pretty-print
rspacefs verity inspect base.manifest.json
```

### `rspacefs-mount` — real FUSE mount (Linux)

```sh
# Plain overlay
sudo rspacefs-mount \
  --upper /var/lib/myapp/upper \
  --lower /var/lib/myapp/app \
  --lower /var/lib/myapp/base \
  /mnt/myroot

# Overlay with a verity-protected base
sudo rspacefs-mount \
  --upper /var/lib/myapp/upper \
  --lower-verified /var/lib/myapp/base \
  --lower /var/lib/myapp/app \
  /mnt/myroot

# Stop: fusermount -u /mnt/myroot   (or just kill the foreground process)
```

The binary runs in the foreground, supervises the FUSE channel, and exits
cleanly on `fusermount -u` or signal. `--auto-unmount` is on by default so
crashes don't leave a half-attached mount.

Build requires Linux. On a Mac dev box the workspace still builds — the
binary compiles to a stub that errors out at runtime.

## Workspace layout

```
rspacefs/
├── crates/
│   ├── rspacefs-core/     # OverlayFS impl
│   ├── rspacefs-verity/   # Merkle / verity impl
│   ├── rspacefs-cli/      # `rspacefs` CLI binary
│   └── rspacefs-fuse/     # `rspacefs-mount` FUSE daemon (Linux)
```

## Build & test

```sh
make build      # cargo build --workspace --release
make test       # cargo test --workspace
make install    # install rspacefs CLI to ~/.cargo/bin
```

Tests are pure unit + integration tests over `MemoryFS`; no privileges needed.

## Design notes

- **No async.** The `vfs::FileSystem` trait is synchronous; that decision
  flows through. If you need async, wrap calls in your runtime's
  `spawn_blocking` — we do not want to force `tokio` into every consumer.
- **Copy-up cost.** First write to a lower-layer file copies the full file
  content into upper. Same as kernel overlayfs; profile before optimizing
  with sparse files / reflinks.
- **Whiteout scan cost.** `is_whiteout` does a per-entry existence check.
  Adequate for typical OCI layer counts (< 20); revisit if profiling shows
  hotspots.
- **Verity verification cost.** First read of each 4 KB block does a
  log₂(n) Merkle walk (~10 SHA-256s for a 4 MB layer). Subsequent reads
  hit the lock-free verified-block bitset (~1 ns).

## Status

`0.1.0` — extracted from nextnfs `0.13.x`. API surface stable; semantic
versioning applies.

## License

MIT. See [LICENSE](LICENSE).
