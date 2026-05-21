# Kubernetes Workloads — Beatup + Benchmarks

Workloads that exercise the single-node cluster brought up by
`../single-node-install/`. Every container pull and start funnels image
mounts through `rspacefs-mount` (CRI-O's `mount_program`), so these are
the real end-to-end tests.

## Layout

| File | Purpose |
|---|---|
| `beatup.sh`              | Orchestrator. Pulls 50 images, runs ~200 short-lived pods, cleans up, captures stats. |
| `beatup-images.txt`      | One image ref per line — the workload set. |
| `bench-startup.sh`       | Time `kubectl run` → "ready" for N images, write CSV. |
| `bench-concurrent-open.yaml`  | DaemonSet/Pod set that simultaneously opens one shared lower-layer file. |
| `bench-metadata-ops.yaml`     | Pod that runs `find / -xdev` repeatedly through the mount. |
| `collect.sh`             | Snapshot: rspacefs-mount RSS, /proc/mountinfo, kubelet/crio logs. |

All of these assume `kubectl` is on PATH and a kubeconfig is loaded.

## Order of operations

1. Bring up cluster: `sudo install-all.sh`
2. Smoke verify: `06-validate.sh` (already part of install-all)
3. Beatup: `./beatup.sh`
4. Benchmarks: `./bench-startup.sh && kubectl apply -f bench-*.yaml`
5. Collect: `./collect.sh > /tmp/rspacefs-k8s-run-$(date +%s).log`
