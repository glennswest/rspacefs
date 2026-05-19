# Changelog

## [Unreleased]

## [v0.1.0] — 2026-05-19

### Added
- Initial extraction from nextnfs `0.13.x`.
- **rspacefs-core** — userspace OverlayFS implementing `vfs::FileSystem`:
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
  `OverlayFS`, proving the two crates compose correctly).
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
