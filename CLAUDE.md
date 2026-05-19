# CLAUDE.md — rspacefs Project

Pure-Rust userspace OverlayFS + dm-verity. Extracted from nextnfs on
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
| `rspacefs-core`    | `crates/rspacefs-core/`    | ~1030 | OverlayFS impl: upper + N lower layers, OCI whiteouts, copy-up.         |
| `rspacefs-verity`  | `crates/rspacefs-verity/`  | ~1390 | SHA-256 Merkle tree, layer manifest, verified-block cache, verified FS. |
| `rspacefs-cli`     | `crates/rspacefs-cli/`     | ~250  | `rspacefs` binary: `overlay {ls,cat,stat}`, `verity {build,verify,inspect}`. |

Both library crates implement `vfs::FileSystem` and compose freely — a
verified read-only layer can be a lower layer of an overlay, an overlay
can be a lower layer of another overlay, etc.

### Dependencies

`rspacefs-core` — only `vfs`.
`rspacefs-verity` — `vfs`, `sha2`, `serde`, `serde_json`, `tracing`.
`rspacefs-cli` — both library crates plus `clap`, `anyhow`.

The verity crate's tests also dev-depend on `rspacefs-core` for the
"verified layer beneath overlay" cross-test, which proves the two crates
compose correctly.

## Cross-project relationship

- **nextnfs** — original home. As of `0.13.x`, the modules are removed from
  nextnfs; nextnfs does **not** depend on rspacefs.
- **mkube** / **stormbase** — anticipated future consumers for container
  rootfs assembly.

If another project wants to use rspacefs, add `rspacefs-core` (and
optionally `rspacefs-verity`) as a Cargo dependency. No NFS server, no
binary, no daemon — it's a library.

## Work Plan

### Current Version: `v0.1.0`

### TODO (priority order)

1. **Publish to crates.io.** Both library crates + the CLI.
2. **Examples.** `examples/overlay_mount.rs`, `examples/verity_build.rs`,
   `examples/verified_overlay.rs`.
3. **Whiteout caching.** Profile `is_whiteout` on directories with many
   entries; cache the per-directory whiteout set if it shows up.
4. **Sparse / reflink copy-up.** On filesystems that support it (XFS, btrfs,
   apfs), use `copy_file_range` or reflinks instead of full content copy.
5. **`vfs` trait extensions.** xattrs, ownership, capabilities — needed to
   make this a complete container-rootfs replacement on Linux. Likely
   requires a fork of the `vfs` crate or adding a sibling
   `rspacefs-vfs-ext` crate.
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
  cross-test wrapping `VerifiedFS` as a lower layer of `OverlayFS`.

## File pointers

- Workspace manifest: `Cargo.toml`
- Overlay source: `crates/rspacefs-core/src/overlay.rs`
- Verity source: `crates/rspacefs-verity/src/verity.rs`
- CLI source: `crates/rspacefs-cli/src/main.rs`
- Original extraction spec: `nextnfs/enhancements/extract-rspacefs.md`
