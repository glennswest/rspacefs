# CLAUDE.md — rspacefs Project

Pure-Rust userspace LayerFS + dm-verity for container image rootfs.
Mounts via FUSE; plugs into containers-storage as `mount_program` so
CRI-O / podman delegate every image mount to rspacefs.

## Issue-first practice

**Every bug we find or fix gets a GitHub Issue.** No exceptions, even
if the fix lands in the same PR. Open it, link the commit, close it.
Reasons:

1. The project's bug-tracking history IS the project's track record.
   "We've found and fixed N real bugs" is more credible than "trust us
   it works."
2. Future-someone debugging a regression needs to find prior context;
   the commit message alone is not searchable from a stack trace.
3. Anyone evaluating rspacefs against alternatives can read the issue
   tracker and see actual operational behavior, not just feature claims.

Workflow:
1. As soon as a bug is identified, file an issue with `gh issue create`
   describing **summary, root cause, live observation, severity, fix**.
2. Fix it. Reference the issue in the commit message: `closes #N` or
   `refs #N`.
3. Close the issue with `gh issue close N --reason completed --comment
   "Fixed in <sha>"`.

Labels in use: `bug`, `enhancement`, `fixed`, `core`, `fuse`,
`tests/k8s`. Add more as needed.

## Build & Test

```bash
cargo build --workspace --release    # build all four crates
cargo test  --workspace              # run all tests
cargo install --path crates/rspacefs-cli   # install the `rspacefs` CLI

make build       # same as cargo build --workspace --release
make test        # same as cargo test --workspace
make install     # cargo install ...
make fmt         # cargo fmt --all
make clippy      # cargo clippy --workspace --all-targets -- -D warnings
```

Synchronous file I/O via the `vfs` trait. No async, no tokio.

## Architecture

| Crate              | Path                       | Purpose                                                                 |
|--------------------|----------------------------|-------------------------------------------------------------------------|
| `rspacefs-core`    | `crates/rspacefs-core/`    | LayerFS impl: upper + N lower layers, OCI whiteouts, copy-up. Whiteout cache makes 150+ layer stacks O(1) after warmup. |
| `rspacefs-verity`  | `crates/rspacefs-verity/`  | SHA-256 Merkle tree, layer manifest, verified-block cache, streaming `VerifiedReader`. Tamper detection at block granularity. |
| `rspacefs-cli`     | `crates/rspacefs-cli/`     | `rspacefs` binary: `overlay {ls,cat,stat}`, `verity {build,verify,inspect}`, `ctl {ping,status,invalidate,stats,info,ops}`. |
| `rspacefs-fuse`    | `crates/rspacefs-fuse/`    | `rspacefs-mount` Linux daemon: real FUSE mount, mount_program-compatible argv, self-daemonizing fork, FUSE_PASSTHROUGH on kernel ≥ 6.9. |

Both library crates implement `vfs::FileSystem` and compose freely — a
verified read-only layer can be a lower layer of an overlay, an overlay
can be a lower layer of another overlay, etc.

### Dependencies

- `rspacefs-core` — only `vfs`.
- `rspacefs-verity` — `vfs`, `sha2`, `serde`, `serde_json`, `tracing`.
- `rspacefs-cli` — both library crates plus `clap`, `anyhow`.
- `rspacefs-fuse` — both library crates plus `fuser` (0.16 with `abi-7-40`
  for FUSE_PASSTHROUGH), `libc`, `xattr`, `clap`, `anyhow`,
  `tracing-subscriber`. `fuser` and `libc` are gated by
  `cfg(target_os = "linux")`; on macOS the workspace still builds and
  produces a stub binary that errors at runtime.

### Building rspacefs-fuse

`fuser` 0.15+ has a build-script that panics when host OS is non-Linux
(uses `cfg!(target_os)` in build.rs against host, not target). That means:

- **macOS** — `cargo build --workspace` works; the fuse crate compiles a stub.
  Cross-compiling to Linux from a Mac fails inside fuser's build.rs.
- **Linux host** — `cargo build --workspace` builds everything including
  the real FUSE mount binary. We disable `fuser`'s defaults at the
  workspace level so it uses its pure-Rust mount path. No system libfuse
  needed at build time; only `/dev/fuse` access at runtime.

To build the FUSE binary, copy/clone the repo onto a Linux host (e.g.,
`test1.g8.lo`) and run:

```sh
cargo build --release -p rspacefs-fuse
# → target/release/rspacefs-mount
```

## How rspacefs fits into a container runtime

```
   ┌────────────────────────────────────────────────────────┐
   │  podman / CRI-O                                        │
   │  (containers-storage with mount_program = rspacefs)    │
   └──────────────────────────┬─────────────────────────────┘
                              │ argv: -o lowerdir=L1:L2,upperdir=U,workdir=W /merged
                              ▼
                       rspacefs-mount    (Linux FUSE daemon)
                              │
                              ▼
         LayerFS (upper + N lowers, whiteouts, copy-up)
                              │
                              ▼
              VerifiedFS (per-layer block hashing)
                              │
                              ▼
                  `vfs::FileSystem` impls
                              │
                              ▼
                 Real filesystem (PhysicalFS)
```

CRI-O extracts each image layer tarball into a directory under
`/var/lib/containers/storage/overlay/l/<id>`. When a container starts,
containers-storage invokes `mount_program` with the upper/lower/workdir
arguments and a mountpoint. rspacefs-mount answers by FUSE-mounting a
LayerFS-backed merged view at that mountpoint, then the container
runtime `pivot_root`s the container into it.

## Sibling projects

- `../rspace_registry/` — Rust OCI Distribution registry head. Sibling
  Cargo workspace. Long-term: shares the same containers-storage
  substrate rspacefs reads, so push lands bytes once and pull serves
  them with zero copy. Integration spec in `enhancements/rspacefs-registry-head.md`.

## Targets

- **OpenShift / Kubernetes / podman on Linux.** Primary target.
  Plugs into containers-storage via `mount_program`. No CRD, no
  controller; one binary on the node + a `storage.conf` line.

## Work Plan

### Current Version: `v0.1.0`

### TODO (priority order)

1. **K8s test cluster up clean on Fedora 42** (reimage of test1.g8.lo
   in progress). Then run beatup + benchmarks end-to-end and commit
   the first results under `tests/k8s/runs/`.
2. **Stats wiring + Prometheus `/metrics` HTTP endpoint** —
   `crates/rspacefs-fuse/src/stats.rs` exists; `record(...)` calls
   landed in most ops. Next: add `--metrics-addr` HTTP listener that
   serves `render_prom()` output, plus a `rspacefs-node-exporter`
   binary that aggregates per-PID sockets into one node-level
   `/metrics`. OpenShift ServiceMonitor manifest in
   `docs/openshift-metrics.md`.
3. **pprof `/debug/pprof/*` endpoint** alongside `/metrics` via
   `pprof-rs` (Go-compatible profile format → `go tool pprof`).
4. **Deep-layer test containers** — `tests/k8s/workloads/deep-layers/`
   `build-set.sh` already generates 100/130/150/200-layer OCI images.
   Run on the live cluster and confirm rspacefs serves layer counts
   that kernel overlay's default 125-layer mount-stack limit rejects.
5. **Pluggable store backend (control socket)** — current control
   surface has `ping`, `status`, `invalidate`. Add `stats`,
   `metrics-text`, `info`, `ops`, `debug` so a single Unix socket
   exposes the full operational view.
6. **Streaming verified reads scale test.** `VerifiedReader` is O(1)
   memory by design — benchmark a 2 GB verified shared lib through the
   mount and confirm the daemon RSS stays flat.
7. **fs-verity descriptor attachment.** Build manifests in a form
   CRI-O can hand to `FS_IOC_ENABLE_VERITY` for in-kernel verification
   of post-extract layer contents. Out of scope of the daemon's
   verification, complementary.
8. **SELinux policy upstream.** Installer flips SELinux to permissive
   today; ship a proper policy module so we can stay enforcing.
9. **Fuzz testing.** Path-traversal, malformed whiteout names,
   oversized trees. `cargo-fuzz` targets.
10. **CLI: overlay write.** `rspacefs overlay write --upper ... <path> -`
    to commit data through the merged view. Useful for scripted layer
    surgery.

### In Progress

- [ ] (started 2026-05-21) Bring up single-node K8s on test1 with
  rspacefs as `mount_program`. Blocked on Fedora 42 reimage.
- [ ] Stats wiring across FUSE ops.

### Recently Completed

- [x] (2026-05-21) Self-deadlock fix in `lower_is_opaque_above` — read
  guard's temporary scope under Rust 2021 edition kept the read lock
  alive into the write-lock branch. Single-threaded fuser thread
  deadlocked on its own RwLock. Caught live on a 14-layer apiserver
  mount during kubeadm bootstrap. Regression test added.
- [x] (2026-05-21) Production-quality single-node K8s installer
  (`tests/k8s/single-node-install/`). Idempotent bash scripts that
  bootstrap upstream Kubernetes + CRI-O 1.32 + rspacefs as
  mount_program + flannel CNI on a Fedora 42 host.
- [x] (2026-05-21) Test history + env-capture system. Every install
  drops a snapshot under `tests/k8s/runs/<id>/` with kernel,
  packages, rspacefs binary sha256, kubelet/crio versions, configs,
  and cluster state. `recreate-env.sh` reinstalls a matching env on
  a fresh host. `HISTORY.md` is auto-appended.
- [x] (2026-05-21) rspacefs-mount self-daemonizes in mount_program
  mode — forks, parent polls statfs() for FUSE_SUPER_MAGIC, child
  detaches via setsid+stdio-to-/dev/null. Containers-storage
  contract honored without an external shim.
- [x] (2026-05-19) FUSE_PASSTHROUGH for read-only non-verified opens
  (kernel ≥ 6.9) — kernel reads backing fd directly, daemon out of
  the hot path.
- [x] (2026-05-19) Streaming `VerifiedReader` — 4 KB block at a time,
  arbitrary `Read`/`Seek`, O(1) memory regardless of file size.
- [x] (2026-05-19) xattr support, reflink copy-up via FICLONE,
  symlink support, pinned verity manifest load, file mode/uid/gid
  preservation via side-channel `stat()` on the physical backing.
- [x] (2026-05-19) Whiteout cache — per-(layer, parent_dir) HashSet
  built lazily and Arc-shared. 150+ layer stacks resolve in O(1)
  after warmup instead of O(N) per lookup.

## Testing

```bash
cargo test --workspace                              # everything
cargo test -p rspacefs-core                         # LayerFS
cargo test -p rspacefs-verity                       # verity (incl. cross-test using rspacefs-core)
cargo test -p rspacefs-cli                          # CLI smoke tests
cargo test -p rspacefs-fuse                         # FUSE-side (Linux only)
```

End-to-end K8s tests under `tests/k8s/`:

```bash
tests/k8s/single-node-install/install-all.sh       # bootstrap real K8s on Fedora 42
tests/k8s/workloads/beatup.sh                      # 50 images, ~200 short-lived pods
tests/k8s/workloads/bench-startup.sh               # container-start latency CSV
tests/k8s/workloads/deep-layers/build-set.sh 150   # generate a 150-layer test image
tests/k8s/env-capture/capture-env.sh --purpose ... # snapshot the host
```

## File pointers

- Workspace manifest: `Cargo.toml`
- LayerFS: `crates/rspacefs-core/src/layer.rs`
- Verity: `crates/rspacefs-verity/src/verity.rs`
- CLI: `crates/rspacefs-cli/src/main.rs`
- FUSE daemon: `crates/rspacefs-fuse/src/main.rs` + `fs.rs` + `control.rs` + `stats.rs`
- K8s installer: `tests/k8s/single-node-install/`
- K8s workloads: `tests/k8s/workloads/`
- Test run history: `tests/k8s/runs/HISTORY.md`
- Enhancement specs: `enhancements/`
- OpenShift integration spec: `docs/openshift-integration.md`
