# Deep-Layer Test Images

Multiple OCI image sets engineered to **break** classic overlayfs and
prove rspacefs handles them. Each set has progressively more layers:

| Set | Layers | What it stresses |
|-----|--------|------------------|
| `set-100`  | 100  | Baseline. Inside kernel overlayfs's historical safe range; verifies our cache still wins. |
| `set-130`  | 130  | **Past the kernel default 125-layer mount-stack limit.** Kernel overlay fails outright; fuse-overlayfs handles it but with noticeable resolve fan-out. |
| `set-150`  | 150  | Matches the design target ("Support 150 layers without slowdown" in CLAUDE.md). |
| `set-200`  | 200  | Pathological. Tests the whiteout cache under heavy worst-case. |

Within each set, each layer adds:
- One small unique file `/data/layer-NNN.txt` (proves layer-N visibility through the merged view)
- One "overlapping" file `/etc/profile.d/shared.sh` (a write that gets shadowed by the next layer — proves top-layer-wins)
- Every 10th layer adds a whiteout (`.wh.layer-N.txt` for some lower N) — proves whiteout propagation under depth

The generator script (`build-set.sh`) uses Buildah (or `podman build --layers`) to assemble each set from a tiny scratch base, push to the local in-cluster registry, and emit a JSON manifest listing the digests + counts. The K8s test driver (`run-deep.sh`) pulls each set sequentially through CRI-O on the test node, runs a verifier container that walks the merged view, and compares against `fuse-overlayfs` baseline.

## Usage

```bash
# On the build host (Mac or test node — both work):
./build-set.sh 100   # builds set-100
./build-set.sh 130   # builds set-130
./build-set.sh 150
./build-set.sh 200

# On the K8s test node:
./run-deep.sh                 # runs all four sets through rspacefs
./run-deep.sh --compare fuse  # runs both rspacefs and fuse-overlayfs side-by-side
```

## What "breaks original overlayfs" means here

Kernel overlayfs has a compile-time cap (default `OVERLAY_MAX_LAYERS=500` on modern kernels but **enforced as a per-mount stack-depth limit at ~125 in practice** for nested mounts). When CRI-O hands a 130+ layer image to kernel overlay, the mount call returns `EBUSY` or `EINVAL`. fuse-overlayfs has no hard cap but performance degrades roughly O(N) per lookup. rspacefs's whiteout-cache makes resolve O(1) after warmup regardless of N.

The deep-layer sets are the unambiguous "this only runs because of rspacefs" demo.
