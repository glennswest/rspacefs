# CLAUDE.md — rspacefs Project

Pure-Rust userspace LayerFS + dm-verity. Extracted from nextnfs on
2026-05-19 so callers can use the layered-rootfs primitives without any
NFS / network / kernel-module dependency.

## Build & Test

```bash
cargo build --workspace --release    # build all three crates
cargo test  --workspace              # run all tests (overlay, verity, cross)
cargo install --path crates/rspacefs-cli   # install the `rspacefs` CLI

make build       # same as cargo build --workspace --release
make test        # same as cargo test --workspace
make install     # cargo install ...
make fmt         # cargo fmt --all
make clippy      # cargo clippy --workspace --all-targets -- -D warnings
```

The whole workspace is `no_std`-friendly in spirit (no async, no tokio, no
networking) but does use `std` for file I/O via the `vfs` crate.

## Architecture

| Crate              | Path                       | Lines | Purpose                                                                 |
|--------------------|----------------------------|-------|-------------------------------------------------------------------------|
| `rspacefs-core`    | `crates/rspacefs-core/`    | ~1030 | LayerFS impl: upper + N lower layers, OCI whiteouts, copy-up.         |
| `rspacefs-verity`  | `crates/rspacefs-verity/`  | ~1390 | SHA-256 Merkle tree, layer manifest, verified-block cache, verified FS. |
| `rspacefs-cli`     | `crates/rspacefs-cli/`     | ~250  | `rspacefs` binary: `overlay {ls,cat,stat}`, `verity {build,verify,inspect}`. |
| `rspacefs-fuse`    | `crates/rspacefs-fuse/`    | ~500  | `rspacefs-mount` Linux daemon: real FUSE mount of a LayerFS, with optional verified lowers. Stub binary on non-Linux. |

Both library crates implement `vfs::FileSystem` and compose freely — a
verified read-only layer can be a lower layer of an overlay, an overlay
can be a lower layer of another overlay, etc.

### Dependencies

`rspacefs-core` — only `vfs`.
`rspacefs-verity` — `vfs`, `sha2`, `serde`, `serde_json`, `tracing`.
`rspacefs-cli` — both library crates plus `clap`, `anyhow`.
`rspacefs-fuse` — both library crates plus `fuser`, `libc`, `clap`, `anyhow`,
`tracing-subscriber`. `fuser` and `libc` are gated by
`cfg(target_os = "linux")`; on macOS the workspace still builds and
produces a stub binary that errors at runtime.

The verity crate's tests also dev-depend on `rspacefs-core` for the
"verified layer beneath overlay" cross-test, which proves the two crates
compose correctly.

### Building rspacefs-fuse

`fuser` 0.15 has a build-script that panics when host OS is non-Linux
(uses `cfg!(target_os)` in build.rs against host, not target). That means:

- **macOS** — `cargo build --workspace` works; the fuse crate compiles a stub.
  Cross-compiling to Linux from a Mac fails inside fuser's build.rs.
- **Linux host** — `cargo build --workspace` builds everything including
  the real FUSE mount binary. Default `fuser` features = `["libfuse"]`,
  but we disable defaults at the workspace level (`default-features = false`),
  so fuser uses its pure-Rust mount path on Linux. No system libfuse needed
  at build time; only `/dev/fuse` access at runtime.

To build the FUSE binary, copy/clone the repo onto a Linux host (e.g.,
`test1.g8.lo`) and run:

```sh
cargo build --release -p rspacefs-fuse
# → target/release/rspacefs-mount
```

## Cross-project relationship

rspacefs sits beneath two different consumer paths with intentionally
different transports — pick the right one per environment, don't conflate:

```
                         rspacefs-core (LayerFS) + rspacefs-verity
                              │
              ┌───────────────┼───────────────────────────────┐
              │               │                               │
       rspacefs-fuse   nextnfs (+ rspacefs-core dep)   rspacefs-registry
       (Linux FUSE,    NFSv4 export of a LayerFS,      (extends mkube-registry,
        end-user)      for environments with no FUSE   ships in mkube)
              │               │                               │
              ▼               ▼                               ▼
       general Linux    MikroTik containers              OpenShift / podman
       dev/test         (root-dir = nfs://...)            (standard OCI pull
                                                          → kernel overlayfs)
```

### MikroTik path (NFS)
nextnfs depends on `rspacefs-core` and gains an export type:
`add_rspacefs_export(name, upper, lowers)`. RouterOS containers are created
with `root-dir=nfs://nfs-host/<export>` — no tarball extract, live overlay,
copy-up to per-container upper. Cross-project glue: nextnfs gets the new
export API, mkube switches container creation off `file=tarball` and onto
`root-dir=nfs`. The container backend (RouterOS) already supports NFS-backed
root-dirs.

### OpenShift path (registry only — no NFS in data path)
`mkube-registry` gains an rspacefs storage backend:
- Eager extraction on layer push: blob + extracted directory tree.
- `POST /v1/commit` snapshots a running container's upper into a new layer
  by renaming the upper-dir into the layer pool (no tar).
- Optional `zstd:chunked` / eStargz output for partial-pull-aware clients.
- Optional fs-verity descriptors attached to manifests so CRI-O can hand
  them to the kernel via `FS_IOC_ENABLE_VERITY`.
- Standard OCI distribution v2 on the wire; CRI-O / podman see a normal
  registry, just faster.

NFS does **not** appear in the OpenShift path. The kernel does overlayfs as
usual; rspacefs-registry just delivers layers to the existing container-
storage path more efficiently.

### Library use
If a project wants to use rspacefs directly without NFS or FUSE, add
`rspacefs-core` (and optionally `rspacefs-verity`) as a Cargo dependency.
No NFS server, no binary, no daemon — it's a library.

## Work Plan

### Current Version: `v0.1.0`

### TODO (priority order)

1. **Symlink support in `rspacefs-fuse`** — `readlink`/`symlink` FUSE ops
   plus a side-channel to `std::os::unix::fs` (vfs trait has no symlink
   methods). Required for any real container image; `/sbin → /usr/sbin`
   and the like break without it.
2. **Pinned verity manifest load** — `--manifest base.manifest.json` on
   `rspacefs-mount` and `LayerFS` callers; load the prebuilt manifest from
   disk instead of rebuilding from current content. Without this, tampering
   between mounts goes undetected.
3. **nextnfs extension** — add `rspacefs-core` as a dep in nextnfs; new
   `add_rspacefs_export(name, upper, lowers)` API on `ExportManagerHandle`.
   Cross-project: spec lands in `nextnfs/enhancements/`. Unblocks MikroTik
   container `root-dir=nfs://…` flow.
4. **mkube-registry rspacefs backend** (in mkube): eager extraction on push
   (blob + extracted dir tree), `POST /v1/commit` for snapshot-container-
   as-image, optional `zstd:chunked` + eStargz output, optional fs-verity
   descriptors in manifests. Spec lands in `mkube/enhancements/`.
5. **Publish to crates.io.** Both library crates + the CLI; FUSE crate
   separately once tested on a few distros.
6. **Examples.** `examples/overlay_mount.rs`, `examples/verity_build.rs`,
   `examples/verified_overlay.rs`.
7. **Whiteout caching.** Profile `is_whiteout` on directories with many
   entries; cache the per-directory whiteout set if it shows up.
8. **Sparse / reflink copy-up.** On filesystems that support it (XFS, btrfs,
   apfs), use `copy_file_range` or reflinks instead of full content copy.
9. **`vfs` trait extensions.** xattrs, ownership, capabilities — needed to
   make this a complete container-rootfs replacement on Linux. Likely
   requires a fork of the `vfs` crate or adding a sibling
   `rspacefs-vfs-ext` crate.
10. **Streaming reads in `rspacefs-fuse`** — current `open()` reads the
    whole file into the file-handle cache. Replace with a `SeekAndRead`
    held in the fh table; serve `read(ino, offset, size)` from that.
11. **FUSE-over-io_uring** — when `fuser` exposes the feature flag, enable
    it. Expected 2–3× metadata throughput.
12. **FUSE passthrough** (kernel ≥ 6.9) — for unmodified lower-layer files,
    register the backing fd as a passthrough target so kernel serves reads
    directly without bouncing through the daemon. Brings reads to near-
    native kernel-FS speed.
6. **Fuzz testing.** Path-traversal, malformed whiteout names, oversized
   trees. `cargo-fuzz` targets.
7. **CLI: overlay write.** `rspacefs overlay write --upper ... <path> -`
   to commit data through the merged view. Useful for scripted layer
   surgery.

### In Progress

- [x] (started 2026-05-19) Extract overlay + verity from nextnfs into
  standalone rspacefs workspace.

### Recently Completed

- [x] Initial extraction from nextnfs 0.13.x — `nfs/src/server/overlay.rs`
  → `rspacefs-core`, `nfs/src/server/verity.rs` → `rspacefs-verity`, plus
  a fresh CLI in `rspacefs-cli`.

## Testing

```bash
cargo test --workspace                              # everything
cargo test -p rspacefs-core                         # overlay only
cargo test -p rspacefs-verity                       # verity only (incl. cross-test using rspacefs-core)
cargo test -p rspacefs-cli                          # CLI smoke tests (if added)
```

Test inventory at extraction time:

- **rspacefs-core**: 24 tests, all `MemoryFS`-based — basic read/write
  through layers, opaque whiteouts, three-layer stacking, EXDEV-safe move.
- **rspacefs-verity**: 30+ tests covering tree build / verify / serialize /
  manifest JSON / verified-block cache / `FileSystem` integration. Includes
  cross-test wrapping `VerifiedFS` as a lower layer of `LayerFS`.

## File pointers

- Workspace manifest: `Cargo.toml`
- Overlay source: `crates/rspacefs-core/src/overlay.rs`
- Verity source: `crates/rspacefs-verity/src/verity.rs`
- CLI source: `crates/rspacefs-cli/src/main.rs`
- Original extraction spec: `nextnfs/enhancements/extract-rspacefs.md`
