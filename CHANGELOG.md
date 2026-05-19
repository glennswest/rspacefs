# Changelog

## [Unreleased]

### 2026-05-19 (more)
- **feat (verity):** Streaming reads through `VerifiedFS`. New `VerifiedReader` keeps one 4 KB block in memory at a time, verifies each block exactly once when first accessed (or every time when caching is disabled in pinned mode), and serves arbitrary `Read`/`Seek` operations. Old behavior read the whole file into a `Cursor` at open time — fine for `/etc/*` config, catastrophic for `/usr/lib/*.so` and large binaries. Now a 200 MB shared library opens in O(1) memory.
- **feat (fuse):** `fsync` and `flush` FUSE ops. `fsync` forces a buffered-handle write-back to upper before returning OK; `flush` is a no-op (release does the write-back to coalesce). Streaming handles have nothing to sync and return OK directly. Container processes calling `fsync(2)` no longer get `ENOSYS`.

### 2026-05-19 (later still)
- **feat (fuse):** Streaming reads. Read-only opens now hold a `Box<dyn vfs::SeekAndRead + Send>` and seek+read at offset, instead of reading the whole file into a per-handle buffer on `open()`. Writable opens stay buffered (read-modify-write inside a file needs the buffer). Huge memory win for large lower-layer files (shared libs, binaries). Verified lowers still buffer internally (VerifiedFS produces a `Cursor` over the verified bytes) — that's a VerifiedFS limitation; the FUSE layer no longer adds its own duplicate buffer on top.
- **feat (fuse):** Extended attributes — `getxattr`, `listxattr`, `setxattr`, `removexattr`. Implemented via the `xattr` crate against the physical backing file, side-channelling the vfs trait the same way mode bits already did. Write-side ops trigger an automatic copy-up into upper (`ensure_in_upper`) that preserves content, mode, AND xattrs from the source layer. Container-runtime requirement for SELinux contexts and POSIX capabilities to survive `pivot_root`.
- **feat (fuse):** Reflink copy-up via `FICLONE` ioctl. On btrfs / xfs / bcachefs (any FS with the kernel reflink interface) the upper-side copy is an instant COW reference instead of a full byte copy. Silent fallback to `std::fs::copy` on filesystems without reflink (ext4 etc.) — best-effort speed-up, never a hard requirement.
- **feat (core):** Re-export `WHITEOUT_PREFIX` and `OPAQUE_WHITEOUT` constants from the crate root so external code can build / detect whiteout markers without re-deriving the strings.
- **examples:** `overlay_mount` and `verity_build` examples — runnable demos of layer merge / whiteout / copy-up and Merkle tree build / serialize / tamper-detect.

### 2026-05-19 (later)
- **feat (fuse):** Symlink support. `rspacefs-mount` now implements the FUSE `readlink` and `symlink` operations and reports symlinks AS symlinks (not as their targets) in `lookup` / `getattr` / `readdir`. The vfs trait has no symlink methods, so the FUSE adapter bypasses it: `readlink` calls `std::fs::read_link` on the resolved physical path; `symlink` writes a real symlink into the upper layer via `std::os::unix::fs::symlink` (and removes any whiteout that would have masked it). Container images that rely on classic Unix symlinks (e.g. `/sbin → /usr/sbin`) now work through the mount.
- **feat (verity):** `VerifiedFS::load_pinned(inner, manifest_json, tree_bin, on_failure)` — load a previously-built Merkle tree + manifest from disk instead of rebuilding from current content. This is the path you want for real tamper-evidence: build the manifest once at image-build time, ship it alongside the layer, load on mount. Manifest and tree must agree on `root_hash` (verified at load).
- **feat (fuse):** `rspacefs-mount` accepts `--lower-verified-pinned DIR=MANIFEST.json=TREE.bin` (repeatable). Pinned lowers go first in layer priority. Existing `--lower-verified DIR` (rebuild-at-mount) is retained but explicitly documented as "only useful for tampering AFTER mount time."

### 2026-05-19
- **BREAKING:** Renamed `OverlayFS` struct → `LayerFS`, file `overlay.rs` → `layer.rs`, to remove any ambiguity with the kernel `overlayfs` module. The whole point of rspacefs is to *replace* kernel overlayfs in user-space — the type name now reflects that. Public-API users update `use rspacefs_core::OverlayFS` → `use rspacefs_core::LayerFS`. CLI subcommand `rspacefs overlay …` kept as-is (lowercase word remains semantic).
- **feat (fuse):** Preserve POSIX metadata through the FUSE mount. `vfs::VfsMetadata` only carries `file_type` + `len`, so `rspacefs-mount` was synthesising `0o644`/`0o755` for every file — losing the executable bit on every binary in a lower layer and making `execve()` impossible. Fixed by giving `RspacefsFuse` the physical layer paths as a side-channel and `stat()`-ing the real backing file for `FileAttr` (mode, uid, gid, atime/mtime/ctime, nlink, rdev). Content I/O still goes through the layered overlay (so verity verification of lowers continues to apply); only metadata bypasses the lossy vfs trait. This is the difference between "podman can `pivot_root` here" and "podman can actually `execve` the binaries it finds here."
- **feat:** `rspacefs-fuse` crate (binary: `rspacefs-mount`). Real Linux FUSE mount of a `LayerFS` (with optional verity-protected lowers). Implements lookup, getattr, setattr, readdir, mkdir, rmdir, open, read, write, release, create, unlink, rename, statfs — enough for container rootfs use cases like podman/CRI-O. `fuser` and `libc` deps are gated to `cfg(target_os = "linux")` so the workspace still builds on Mac dev boxes (the binary compiles to a stub that errors at runtime with a clear message). Build the real binary on a Linux host: `cargo build -p rspacefs-fuse`.

## [v0.1.0] — 2026-05-19

### Added
- Initial extraction from nextnfs `0.13.x`.
- **rspacefs-core** — userspace LayerFS implementing `vfs::FileSystem`:
  upper + N lower layers, OCI-spec whiteouts (`.wh.` and `.wh..wh..opq`),
  copy-up on write, merged sorted readdir, EXDEV-safe `move_dir`.
- **rspacefs-verity** — SHA-256 Merkle tree over 4 KB blocks,
  `LayerManifest` with serde JSON support, lock-free verified-block bitset,
  `VerifiedLayerVfs` and `VerifiedFS` wrappers (read-only, fail-on-mismatch
  or warn-only).
- **rspacefs-cli** — `rspacefs` binary with `overlay {ls,cat,stat}` and
  `verity {build,verify,inspect}` subcommands.
- Tests carried over verbatim: 24 in rspacefs-core, 30+ in rspacefs-verity
  (includes a cross-test that wraps a `VerifiedFS` as a lower layer of
  `LayerFS`, proving the two crates compose correctly).
- README, CLAUDE.md, Makefile, examples scaffolding.

### Architecture
- Two library crates plus a CLI in a Cargo workspace.
- Zero NFS / network / kernel-module / async dependencies.
- Only deps: `vfs`, `sha2`, `serde`, `serde_json`, `tracing`, plus `clap`
  and `anyhow` for the CLI.

### Relationship to nextnfs
- Code originally lived in `nextnfs/nfs/src/server/{overlay,verity}.rs`.
- nextnfs `0.13.x` drops both modules; nextnfs does **not** depend on
  rspacefs.
- Extraction motivation: layered-rootfs primitives shouldn't require an
  NFS server in the data path. See `nextnfs/enhancements/extract-rspacefs.md`.
