# rspacefs

**A tamper-evident, rootless, user-space container rootfs engine for
OpenShift / Kubernetes — and anywhere else `containers-storage` runs.**

`rspacefs` plugs into [`containers-storage`](https://github.com/containers/storage)
via its standard `mount_program` + `additionalimagestores` extension
points. It replaces (or sits alongside) [`fuse-overlayfs`](https://github.com/containers/fuse-overlayfs)
as the FUSE helper that assembles container rootfs on every node, and
adds per-layer cryptographic verification, a live admin surface, and
streaming reads with kernel-bypass passthrough.

No new APIs, no new CRDs, no kernel module, no `CAP_SYS_ADMIN`. Just one
config line in `/etc/containers/storage.conf`:

```toml
[storage.options]
mount_program = "/usr/bin/rspacefs-mount"
additionalimagestores = ["/var/lib/rspacefs/store"]
```

Then every `oc apply` / `podman run` / `buildah build` on that node
uses rspacefs underneath.

## What you get on every container

| | Without rspacefs (fuse-overlayfs) | **With rspacefs** |
|---|---|---|
| Rootless overlay (no CAP_SYS_ADMIN) | ✅ | ✅ |
| Per-layer cryptographic verification | ❌ | **SHA-256 Merkle tree per layer; every block hashed on read** |
| Mixed trust (verified + plain lowers) | ❌ | **Yes — base+deps verified, app code plain, in one mount** |
| Tamper detection mid-mount | ❌ | **Yes — verified live on Fedora 43 / kernel 6.17 — block-level rejection** |
| FUSE passthrough for non-verified reads | ❌ | **Yes — kernel reads direct from backing fd; daemon idle on the hot path** |
| Streaming verified reads | n/a | **O(1) memory regardless of file size; daemon RSS measured at ~6 MB while serving a 64 MB verified file** |
| Live admin surface (status, invalidate, future verbs) | ❌ | **Yes — `rspacefs ctl` over Unix socket; JSON protocol; verb-extensible** |
| Symlinks, xattrs (SELinux, capabilities), modes preserved | ✅ | **✅** |
| Reflink copy-up on btrfs/xfs | partial | **Yes — FICLONE ioctl with byte-copy fallback** |
| Memory-safe implementation | C | **Rust** |

## Quick install on OpenShift / podman / CRI-O

See [`docs/openshift-integration.md`](docs/openshift-integration.md) for
the full DaemonSet + verity-staging design. Minimal recipe:

```sh
# 1. Drop the binary on each node.
sudo install -m 0755 rspacefs-mount /usr/bin/rspacefs-mount

# 2. Point containers-storage at it.
cat > /etc/containers/storage.conf <<'EOF'
[storage]
driver = "overlay"
[storage.options]
mount_program = "/usr/bin/rspacefs-mount"
additionalimagestores = ["/var/lib/rspacefs/store"]
mountopt = "nodev"
EOF

# 3. Use podman / oc / buildah exactly as you always do.
podman pull docker.io/library/alpine:latest
podman run --rm alpine /bin/sh -c "echo container served by rspacefs"
```

Live-tested on Fedora 43 (kernel 6.17.1) with podman 5 — see
[`tests/integration/podman-mount-program.md`](tests/integration/podman-mount-program.md).

## Architecture

```
                       OpenShift / Kubernetes node
   ┌──────────────────────────────────────────────────────────────┐
   │  CRI-O / podman / buildah                                     │
   │      │                                                        │
   │      ▼ image management (pull, store, mount)                  │
   │  containers-storage  ← reads /etc/containers/storage.conf     │
   │      │                                                        │
   │      │ mount_program = /usr/bin/rspacefs-mount                │
   │      │ additionalimagestores = ["/var/lib/rspacefs/store"]    │
   │      ▼                                                        │
   │  ┌──────────────────────────────────────────────────────────┐ │
   │  │  rspacefs-mount  (FUSE daemon, this repo)                 │ │
   │  │   ├─ rspacefs-core    — LayerFS merge / whiteout / COW    │ │
   │  │   ├─ rspacefs-verity  — SHA-256 Merkle + signed manifest  │ │
   │  │   └─ rspacefs-ctl     — Unix-socket admin surface         │ │
   │  └──────────────────────────────────────────────────────────┘ │
   │      │                                                        │
   │      │ FUSE (with passthrough on kernel ≥ 6.9)                │
   │      ▼                                                        │
   │  Merged rootfs at /var/lib/.../overlay/<id>/merged            │
   │      │                                                        │
   │      │ runtime pivot_root's here                              │
   │      ▼                                                        │
   │  Running container (sees a normal rootfs)                     │
   └──────────────────────────────────────────────────────────────┘
```

`mount_program` is what CRI-O exposes today. A proper CRI-O snapshotter
interface (analogous to containerd's snapshotter model) has been floated
upstream; when it lands, rspacefs becomes a snapshotter plugin. Until
then, `mount_program + additionalimagestores` is the correct integration.

## Crates

| Crate | Role |
|---|---|
| `rspacefs-core` | LayerFS — merge N read-only lower layers with a writable upper, OCI whiteouts (`.wh.<name>`, `.wh..wh..opq`), copy-up on write. |
| `rspacefs-verity` | SHA-256 Merkle tree over 4 KB blocks; `LayerManifest` with serde JSON; lock-free verified-block cache; streaming `VerifiedReader` (no whole-file buffer). |
| `rspacefs-fuse` | `rspacefs-mount` Linux daemon. FUSE adapter supporting passthrough, streaming, symlinks, xattrs, fsync, reflink copy-up, and the `--control-socket` admin surface. Compatible with `containers-storage`'s `mount_program` argv. |
| `rspacefs-cli` | `rspacefs` command-line tool. Subcommands: `overlay ls/cat/stat`, `verity build/verify/inspect`, `ctl ping/status/invalidate`. |

## Use as a Rust library

For tools that want the layered-FS engine directly without a FUSE mount:

```rust
use rspacefs_core::LayerFS;
use rspacefs_verity::{VerifiedFS, OnFailure};
use vfs::{PhysicalFS, VfsPath};

let upper = VfsPath::new(PhysicalFS::new("/var/lib/myapp/upper".into()));
let base = VfsPath::new(PhysicalFS::new("/var/lib/myapp/base".into()));
let base_verified: VfsPath =
    VerifiedFS::load_pinned(base, "base.manifest.json".as_ref(), "base.tree".as_ref(), OnFailure::Reject)?.into();

let merged: VfsPath = LayerFS::new(upper, vec![base_verified]).into();
// merged behaves like a normal vfs::FileSystem — reads are tamper-evident.
```

The library is sync, no async runtime. Builds backends are pluggable
through the [`vfs`](https://crates.io/crates/vfs) trait — `PhysicalFS`,
`MemoryFS`, or your own.

## Build & test

```sh
make build      # cargo build --workspace --release
make test       # cargo test --workspace (71 unit tests + 2 doctests)
make install    # install rspacefs + rspacefs-mount to ~/.cargo/bin
```

Workspace builds on macOS too — the FUSE daemon compiles to a stub
on non-Linux hosts so library development doesn't need a Linux box.

## Live admin surface

```sh
# Start the daemon with a control socket.
rspacefs-mount --upper /u --lower /l --control-socket /run/rspacefs/foo.sock /mnt

# From anywhere on the same host:
rspacefs ctl --socket /run/rspacefs/foo.sock status
rspacefs ctl --socket /run/rspacefs/foo.sock invalidate
```

Current verbs: `ping`, `status`, `invalidate` (issues
`FUSE_NOTIFY_INVAL_ENTRY` for every top-level entry).

Planned verbs (same plumbing, new dispatch arms): `verify <layer>`,
`snapshot <output>`, `swap-lower <index> <path>`,
`reload-manifest <layer> <manifest> <tree>`.

## Status

`0.1.0` — used in development against Fedora 43 + podman 5; production
deployments pending. API surface stable; semantic versioning applies.

Active issues at
[github.com/glennswest/rspacefs/issues](https://github.com/glennswest/rspacefs/issues).

## License

MIT. See [LICENSE](LICENSE).
